//! Shared helpers for integration tests: tiny shell-script "AI apps",
//! artifact builders and a misbehaving HTTP server for network faults.

#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use edgeforge::cgroup::Cgroups;
use edgeforge::fetch::FetchOptions;
use edgeforge::oci::{MT_CONFIG, MT_LAYER_GZIP};
use edgeforge::pack::{self, LayoutFiles};
use edgeforge::spec::{HealthProbe, HealthSpec, ResourceSpec, RestartSpec};
use edgeforge::{AppSpec, Manager, ManagerConfig};
use tempfile::TempDir;

/// Runs forever, like an inference service.
pub const SERVE: &str = "#!/bin/sh\necho \"serving $EDGEFORGE_APP v$EDGEFORGE_VERSION\"\nwhile true; do sleep 0.05; done\n";
/// Crashes immediately, like a build with a missing model file.
pub const CRASH: &str = "#!/bin/sh\necho 'model.onnx not found' >&2\nexit 3\n";

pub struct Env {
    pub tmp: TempDir,
    pub mgr: Arc<Manager>,
}

impl Env {
    pub fn root(&self) -> PathBuf {
        self.tmp.path().join("root")
    }
    pub fn cgroup_root(&self) -> PathBuf {
        self.tmp.path().join("cgroup")
    }
    pub fn app_dir(&self, name: &str) -> PathBuf {
        self.root().join("apps").join(name)
    }
    pub fn artifacts(&self) -> PathBuf {
        self.tmp.path().join("artifacts")
    }

    /// Simulate a daemon restart (e.g. after power loss).
    pub fn reopen(&mut self) {
        self.mgr.shutdown();
        self.mgr = Manager::open(config(self.tmp.path())).unwrap();
    }

    pub fn tick_until(&self, timeout: Duration, mut f: impl FnMut(&Manager) -> bool) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            self.mgr.supervise_tick();
            if f(&self.mgr) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.mgr.shutdown();
    }
}

fn config(tmp: &Path) -> ManagerConfig {
    ManagerConfig {
        root: tmp.join("root"),
        cgroups: Cgroups::at(&tmp.join("cgroup")).unwrap(),
        fetch: FetchOptions {
            timeout: Duration::from_millis(800),
            retries: 1,
            backoff: Duration::from_millis(50),
            max_bytes: 64 * 1024 * 1024,
        },
        stop_grace: Duration::from_millis(500),
    }
}

pub fn env() -> Env {
    let _ = env_logger::builder().is_test(true).try_init();
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Manager::open(config(tmp.path())).unwrap();
    Env { tmp, mgr }
}

/// A spec with fast health checks suitable for tests.
pub fn spec(name: &str, version: &str) -> AppSpec {
    AppSpec {
        name: name.into(),
        version: version.into(),
        entrypoint: vec!["./run.sh".into()],
        env: Default::default(),
        health: HealthSpec {
            probe: HealthProbe::Process,
            interval_ms: 40,
            timeout_ms: 500,
            success_threshold: 3,
            failure_threshold: 2,
            grace_ms: 1_500,
        },
        resources: ResourceSpec::default(),
        restart: RestartSpec {
            max_restarts: 3,
            backoff_ms: 30,
            stable_ms: 60_000,
        },
    }
}

pub fn app_source(env: &Env, spec: &AppSpec, script: &str) -> PathBuf {
    let dir = env
        .tmp
        .path()
        .join("src")
        .join(format!("{}-{}", spec.name, spec.version));
    fs::create_dir_all(dir.join("model")).unwrap();
    let run = dir.join("run.sh");
    fs::write(&run, script).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        dir.join("model/labels.txt"),
        format!("person\ncar\n# {}\n", spec.version),
    )
    .unwrap();
    dir
}

/// Build an artifact and return its path.
pub fn artifact(env: &Env, spec: &AppSpec, script: &str) -> PathBuf {
    let dir = app_source(env, spec, script);
    let out = env
        .artifacts()
        .join(format!("{}-{}.tar", spec.name, spec.version));
    pack::pack_app(&dir, spec, &out).unwrap();
    out
}

/// Build the in-memory layout so a test can tamper with it.
pub fn layout(env: &Env, spec: &AppSpec, script: &str) -> LayoutFiles {
    let dir = app_source(env, spec, script);
    let layer = pack::layer_from_dir(&dir).unwrap();
    LayoutFiles::build(
        &serde_json::to_vec(spec).unwrap(),
        MT_CONFIG,
        &[(layer, MT_LAYER_GZIP)],
        &spec.version,
    )
}

pub fn write(env: &Env, layout: &LayoutFiles, name: &str) -> PathBuf {
    let out = env.artifacts().join(name);
    layout.write_bundle(&out).unwrap();
    out
}

pub fn s(p: &Path) -> String {
    p.display().to_string()
}

pub fn metric(mgr: &Manager, needle: &str) -> f64 {
    mgr.metrics()
        .render()
        .lines()
        .find(|l| l.starts_with(needle))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// How the fake artifact server misbehaves.
#[derive(Clone, Copy)]
pub enum Fault {
    /// Serve the whole file.
    None,
    /// Announce the full Content-Length but close after `n` bytes.
    TruncateAfter(usize),
    /// Accept the connection and never answer.
    Stall,
}

/// Serve `body` on 127.0.0.1 with the given fault. Returns the URL.
pub fn serve(body: Vec<u8>, fault: Fault) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let body = body.clone();
            thread::spawn(move || respond(stream, &body, fault));
        }
    });
    format!("http://{addr}/artifacts/app.tar")
}

fn respond(mut s: TcpStream, body: &[u8], fault: Fault) {
    let mut req = Vec::new();
    let mut buf = [0u8; 1024];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => req.extend_from_slice(&buf[..n]),
        }
    }
    match fault {
        Fault::Stall => thread::sleep(Duration::from_secs(5)),
        Fault::None | Fault::TruncateAfter(_) => {
            let n = match fault {
                Fault::TruncateAfter(n) => n.min(body.len()),
                _ => body.len(),
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(&body[..n]);
            let _ = s.flush();
        }
    }
}

/// A URL on a port where nothing is listening.
pub fn dead_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}/artifacts/app.tar")
}
