//! cgroups v2 resource control.
//!
//! Each app gets its own leaf cgroup under a base directory
//! (`/sys/fs/cgroup/edgeforge/<app>` by default) with `memory.max` and
//! `cpu.max` applied before its pid is moved in. On hosts without a unified
//! cgroup v2 hierarchy, or without permission to write it, EdgeForge keeps
//! running and falls back to `/proc` based accounting.

use std::fs;
use std::path::{Path, PathBuf};

use log::{info, warn};

use crate::error::Result;
use crate::spec::ResourceSpec;

/// The cgroup v2 CPU period EdgeForge uses (100ms, the kernel default).
pub const CPU_PERIOD_US: u64 = 100_000;

#[derive(Debug, Clone)]
pub struct Cgroups {
    base: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CgroupStats {
    pub memory_bytes: u64,
    pub cpu_seconds: f64,
}

/// Format the `cpu.max` value for a millicore limit.
pub fn cpu_max_value(millicores: Option<u32>) -> String {
    match millicores {
        Some(m) => format!("{} {}", m as u64 * CPU_PERIOD_US / 1000, CPU_PERIOD_US),
        None => format!("max {CPU_PERIOD_US}"),
    }
}

pub fn memory_max_value(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "max".to_string(), |b| b.to_string())
}

impl Cgroups {
    pub fn disabled() -> Self {
        Cgroups { base: None }
    }

    /// Use `base` without probing the kernel. Intended for tests and for
    /// hosts with a non-standard mount.
    pub fn at(base: &Path) -> Result<Self> {
        fs::create_dir_all(base)?;
        Ok(Cgroups {
            base: Some(base.to_path_buf()),
        })
    }

    /// Enable cgroups if `base`'s parent is a cgroup v2 directory we can
    /// create children in; otherwise return a disabled instance.
    pub fn detect(base: &Path) -> Self {
        let parent = match base.parent() {
            Some(p) => p,
            None => return Self::disabled(),
        };
        if !parent.join("cgroup.controllers").exists() {
            warn!(
                "{} is not a cgroup v2 hierarchy; resource limits disabled",
                parent.display()
            );
            return Self::disabled();
        }
        if let Err(e) = fs::create_dir_all(base) {
            warn!(
                "cannot create {}: {e}; resource limits disabled",
                base.display()
            );
            return Self::disabled();
        }
        // Delegate cpu and memory controllers to per-app children.
        let _ = fs::write(parent.join("cgroup.subtree_control"), "+cpu +memory");
        if let Err(e) = fs::write(base.join("cgroup.subtree_control"), "+cpu +memory") {
            warn!(
                "could not enable cpu/memory controllers in {}: {e}",
                base.display()
            );
        }
        info!("cgroups v2 enabled at {}", base.display());
        Cgroups {
            base: Some(base.to_path_buf()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.base.is_some()
    }

    pub fn path_for(&self, app: &str) -> Option<PathBuf> {
        self.base.as_ref().map(|b| b.join(app))
    }

    /// Create the app cgroup, write limits and move `pid` into it.
    pub fn apply(&self, app: &str, limits: &ResourceSpec, pid: u32) -> Result<Option<PathBuf>> {
        let Some(dir) = self.path_for(app) else {
            return Ok(None);
        };
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("memory.max"),
            memory_max_value(limits.memory_max_bytes),
        )?;
        fs::write(
            dir.join("cpu.max"),
            cpu_max_value(limits.cpu_max_millicores),
        )?;
        fs::write(dir.join("cgroup.procs"), pid.to_string())?;
        Ok(Some(dir))
    }

    pub fn stats(&self, app: &str) -> Option<CgroupStats> {
        let dir = self.path_for(app)?;
        let memory_bytes = fs::read_to_string(dir.join("memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let cpu_usec: u64 = fs::read_to_string(dir.join("cpu.stat"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("usage_usec "))?
            .trim()
            .parse()
            .ok()?;
        Some(CgroupStats {
            memory_bytes,
            cpu_seconds: cpu_usec as f64 / 1e6,
        })
    }

    /// Remove the app cgroup (it must be empty, i.e. the app stopped).
    pub fn remove(&self, app: &str) {
        if let Some(dir) = self.path_for(app) {
            if fs::remove_dir(&dir).is_err() {
                // Plain directories (tests, fallback mounts) need a recursive delete.
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_formats_millicores() {
        assert_eq!(cpu_max_value(Some(250)), "25000 100000");
        assert_eq!(cpu_max_value(Some(1500)), "150000 100000");
        assert_eq!(cpu_max_value(None), "max 100000");
        assert_eq!(memory_max_value(Some(64 << 20)), "67108864");
        assert_eq!(memory_max_value(None), "max");
    }

    #[test]
    fn reads_stats_files() {
        let t = tempfile::tempdir().unwrap();
        let cg = Cgroups::at(t.path()).unwrap();
        let dir = t.path().join("det");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("memory.current"), "52428800\n").unwrap();
        fs::write(
            dir.join("cpu.stat"),
            "usage_usec 2500000\nuser_usec 2000000\nsystem_usec 500000\n",
        )
        .unwrap();
        assert_eq!(
            cg.stats("det"),
            Some(CgroupStats {
                memory_bytes: 52_428_800,
                cpu_seconds: 2.5
            })
        );
    }

    #[test]
    fn detect_without_cgroupfs_is_disabled() {
        let t = tempfile::tempdir().unwrap();
        assert!(!Cgroups::detect(&t.path().join("edgeforge")).enabled());
    }
}
