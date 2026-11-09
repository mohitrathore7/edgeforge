//! Spawning, stopping and measuring app processes.

use std::fs::{self, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{EdgeError, Result};
use crate::spec::AppSpec;

/// A supervised process. Each app runs in its own process group so that
/// stopping it also stops anything its entrypoint spawned.
pub struct ProcHandle {
    child: Child,
    pub pid: u32,
    pub started: Instant,
    exited: Option<ExitStatus>,
}

/// Relative program paths (`./run.sh`, `bin/infer`) resolve inside the slot.
pub fn resolve_program(program: &str, workdir: &Path) -> PathBuf {
    let p = Path::new(program);
    if p.is_relative() && program.contains('/') {
        workdir.join(p)
    } else {
        p.to_path_buf()
    }
}

pub fn spawn(spec: &AppSpec, workdir: &Path, log_file: &Path) -> Result<ProcHandle> {
    if let Some(dir) = log_file.parent() {
        fs::create_dir_all(dir)?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)?;
    let stderr = stdout.try_clone()?;
    let program = resolve_program(&spec.entrypoint[0], workdir);
    let child = Command::new(&program)
        .args(&spec.entrypoint[1..])
        .current_dir(workdir)
        .envs(&spec.env)
        .env("EDGEFORGE_APP", &spec.name)
        .env("EDGEFORGE_VERSION", &spec.version)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|e| EdgeError::Runtime(format!("failed to start {}: {e}", program.display())))?;
    Ok(ProcHandle {
        pid: child.id(),
        child,
        started: Instant::now(),
        exited: None,
    })
}

impl ProcHandle {
    pub fn alive(&mut self) -> bool {
        if self.exited.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.exited = Some(status);
                false
            }
            Err(_) => false,
        }
    }

    pub fn exit_status(&mut self) -> Option<ExitStatus> {
        self.alive();
        self.exited
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// SIGTERM the process group, wait up to `grace`, then SIGKILL.
    pub fn stop(&mut self, grace: Duration) {
        if !self.alive() {
            return;
        }
        let pgid = -(self.pid as i32);
        // SAFETY: plain kill(2) on a process group we created.
        unsafe { libc::kill(pgid, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !self.alive() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: as above.
        unsafe { libc::kill(pgid, libc::SIGKILL) };
        let _ = self.child.kill();
        if let Ok(status) = self.child.wait() {
            self.exited = Some(status);
        }
    }
}

/// Resident memory (bytes) and consumed CPU time (seconds) from `/proc`.
/// Used when the app is not running inside a cgroup.
pub fn proc_stats(pid: u32) -> Option<(u64, f64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // Fields after the command name start at `state` (field 3 in proc(5)).
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let rss_pages: u64 = fields.get(21)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let (ticks, page) = unsafe {
        (
            libc::sysconf(libc::_SC_CLK_TCK),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    if ticks <= 0 || page <= 0 {
        return None;
    }
    Some((
        rss_pages * page as u64,
        (utime + stime) as f64 / ticks as f64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_programs_resolve_into_slot() {
        let w = Path::new("/var/lib/edgeforge/apps/x/slots/a");
        assert_eq!(resolve_program("./run.sh", w), w.join("./run.sh"));
        assert_eq!(resolve_program("python3", w), PathBuf::from("python3"));
        assert_eq!(
            resolve_program("/usr/bin/env", w),
            PathBuf::from("/usr/bin/env")
        );
    }

    #[test]
    fn reads_own_proc_stats() {
        let (rss, cpu) = proc_stats(std::process::id()).expect("linux /proc");
        assert!(rss > 0);
        assert!(cpu >= 0.0);
    }
}
