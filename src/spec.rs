//! The application spec shipped as the OCI config blob
//! (`application/vnd.edgeforge.app.config.v1+json`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{EdgeError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    pub name: String,
    pub version: String,
    /// Program and arguments, relative paths resolve inside the app slot.
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub health: HealthSpec,
    #[serde(default)]
    pub resources: ResourceSpec,
    #[serde(default)]
    pub restart: RestartSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HealthProbe {
    /// Healthy while the process is running.
    #[default]
    Process,
    /// Healthy when the command exits with status 0.
    Exec { command: Vec<String> },
    /// Healthy when `GET url` returns a 2xx status.
    Http { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HealthSpec {
    #[serde(default)]
    pub probe: HealthProbe,
    #[serde(default = "d_interval")]
    pub interval_ms: u64,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    /// Consecutive passing probes required to accept a new version.
    #[serde(default = "d_success")]
    pub success_threshold: u32,
    /// Consecutive failing probes before a running app is restarted.
    #[serde(default = "d_failure")]
    pub failure_threshold: u32,
    /// How long an update may take to become healthy before it is rolled back.
    #[serde(default = "d_grace")]
    pub grace_ms: u64,
}

fn d_interval() -> u64 {
    1_000
}
fn d_timeout() -> u64 {
    2_000
}
fn d_success() -> u32 {
    3
}
fn d_failure() -> u32 {
    3
}
fn d_grace() -> u64 {
    15_000
}

impl Default for HealthSpec {
    fn default() -> Self {
        HealthSpec {
            probe: HealthProbe::Process,
            interval_ms: d_interval(),
            timeout_ms: d_timeout(),
            success_threshold: d_success(),
            failure_threshold: d_failure(),
            grace_ms: d_grace(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct ResourceSpec {
    /// Hard memory limit written to `memory.max`.
    #[serde(default)]
    pub memory_max_bytes: Option<u64>,
    /// CPU limit in millicores (1000 = one full core) written to `cpu.max`.
    #[serde(default)]
    pub cpu_max_millicores: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RestartSpec {
    /// Crashes tolerated before the app is parked in `crash_loop`.
    #[serde(default = "d_max_restarts")]
    pub max_restarts: u32,
    /// Base delay for exponential restart backoff.
    #[serde(default = "d_backoff")]
    pub backoff_ms: u64,
    /// Uptime after which the crash counter resets.
    #[serde(default = "d_stable")]
    pub stable_ms: u64,
}

fn d_max_restarts() -> u32 {
    5
}
fn d_backoff() -> u64 {
    1_000
}
fn d_stable() -> u64 {
    30_000
}

impl Default for RestartSpec {
    fn default() -> Self {
        RestartSpec {
            max_restarts: d_max_restarts(),
            backoff_ms: d_backoff(),
            stable_ms: d_stable(),
        }
    }
}

impl AppSpec {
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(EdgeError::Verify(format!("invalid app spec: {m}")));
        if !valid_name(&self.name) {
            return bad(format!(
                "name '{}' must match [a-z0-9][a-z0-9_-]{{0,62}}",
                self.name
            ));
        }
        if self.version.is_empty()
            || self.version.len() > 64
            || self.version.contains(['/', '\\', ' '])
        {
            return bad(format!("version '{}' is not valid", self.version));
        }
        if self.entrypoint.is_empty() || self.entrypoint[0].is_empty() {
            return bad("entrypoint must not be empty".into());
        }
        let h = &self.health;
        if h.interval_ms == 0 || h.timeout_ms == 0 || h.success_threshold == 0 {
            return bad("health interval, timeout and success_threshold must be > 0".into());
        }
        match &h.probe {
            HealthProbe::Exec { command } if command.is_empty() => {
                return bad("exec probe needs a command".into())
            }
            HealthProbe::Http { url } if crate::health::parse_http_url(url).is_none() => {
                return bad(format!("http probe url '{url}' is not a valid http:// url"))
            }
            _ => {}
        }
        if self.resources.cpu_max_millicores == Some(0) {
            return bad("cpu_max_millicores must be > 0".into());
        }
        if matches!(self.resources.memory_max_bytes, Some(m) if m < 1024 * 1024) {
            return bad("memory_max_bytes must be at least 1 MiB".into());
        }
        Ok(())
    }
}

pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AppSpec {
        serde_json::from_str(r#"{"name":"detector","version":"1.2.0","entrypoint":["./run.sh"]}"#)
            .unwrap()
    }

    #[test]
    fn defaults_are_filled_in() {
        let s = spec();
        assert_eq!(s.health.probe, HealthProbe::Process);
        assert_eq!(s.health.success_threshold, 3);
        assert_eq!(s.restart.max_restarts, 5);
        s.validate().unwrap();
    }

    #[test]
    fn rejects_bad_names_and_versions() {
        let mut s = spec();
        s.name = "../etc".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.version = "1.0/evil".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let r: std::result::Result<AppSpec, _> = serde_json::from_str(
            r#"{"name":"a","version":"1","entrypoint":["x"],"privileged":true}"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn parses_tagged_probes() {
        let s: AppSpec = serde_json::from_str(
            r#"{"name":"a","version":"1","entrypoint":["x"],
                "health":{"probe":{"kind":"http","url":"http://127.0.0.1:8080/healthz"}}}"#,
        )
        .unwrap();
        assert!(matches!(s.health.probe, HealthProbe::Http { .. }));
        s.validate().unwrap();
    }
}
