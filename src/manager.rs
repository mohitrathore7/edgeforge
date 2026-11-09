//! App lifecycle: install, start, stop, restart, update, rollback, remove.
//!
//! On-disk layout per app:
//!
//! ```text
//! <root>/apps/<name>/
//!   slots/a/            unpacked files of one version
//!   slots/b/            unpacked files of the other version
//!   current -> slots/a  symlink swapped atomically on update / rollback
//!   meta/a.json         spec + manifest digest of each slot
//!   state.json          active / previous slot and any in-flight update
//!   logs/app.log        stdout + stderr of the app
//! ```
//!
//! An update writes the new version into the *inactive* slot, records a
//! `pending` transaction, swaps `current`, and starts the new version behind a
//! health gate. If the new version crashes or never becomes healthy within
//! `health.grace_ms`, `current` is swapped back and the old version restarted.
//! If the device loses power mid-update, the pending transaction is found on
//! the next boot and rolled back to the last known-good slot.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{error, info, warn};
use serde::{Deserialize, Serialize};

use crate::cgroup::Cgroups;
use crate::error::{EdgeError, Result};
use crate::fetch::{self, FetchOptions};
use crate::fsutil;
use crate::health::{self, ProbeResult};
use crate::metrics::Metrics;
use crate::oci::{self, VerifiedImage};
use crate::process::{self, ProcHandle};
use crate::spec::AppSpec;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlotRef {
    pub slot: Slot,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingUpdate {
    pub from: SlotRef,
    pub to: SlotRef,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub name: String,
    pub active: SlotRef,
    #[serde(default)]
    pub previous: Option<SlotRef>,
    #[serde(default)]
    pub pending: Option<PendingUpdate>,
    pub desired_running: bool,
    pub installed_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotMeta {
    pub spec: AppSpec,
    pub manifest_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Unknown,
    Starting,
    Healthy,
    Unhealthy,
    Stopped,
    CrashLoop,
    /// The app is locked by a long-running operation (e.g. an update's health gate).
    Busy,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppStatus {
    pub name: String,
    pub version: String,
    pub slot: Option<Slot>,
    pub previous_version: Option<String>,
    pub manifest_digest: Option<String>,
    pub desired_running: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub health: Health,
    pub restarts: u32,
    pub memory_bytes: Option<u64>,
    pub cpu_seconds: Option<f64>,
    pub cgroup: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone)]
pub struct ManagerConfig {
    pub root: PathBuf,
    pub cgroups: Cgroups,
    pub fetch: FetchOptions,
    /// SIGTERM -> SIGKILL grace period when stopping apps.
    pub stop_grace: Duration,
}

impl ManagerConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ManagerConfig {
            root: root.into(),
            cgroups: Cgroups::disabled(),
            fetch: FetchOptions::default(),
            stop_grace: Duration::from_secs(10),
        }
    }
}

struct App {
    dir: PathBuf,
    state: AppState,
    spec: AppSpec,
    manifest_digest: String,
    proc: Option<ProcHandle>,
    health: Health,
    restarts: u32,
    crashes: u32,
    next_restart_at: Option<Instant>,
    last_probe: Option<Instant>,
    probe_failures: u32,
    cgroup: Option<PathBuf>,
    last_error: Option<String>,
}

pub struct Manager {
    cfg: ManagerConfig,
    metrics: Arc<Metrics>,
    apps: RwLock<BTreeMap<String, Arc<Mutex<App>>>>,
    staging_seq: AtomicU64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn slot_dir(app_dir: &Path, slot: Slot) -> PathBuf {
    app_dir.join("slots").join(slot.as_str())
}
fn meta_path(app_dir: &Path, slot: Slot) -> PathBuf {
    app_dir.join("meta").join(format!("{}.json", slot.as_str()))
}
fn state_path(app_dir: &Path) -> PathBuf {
    app_dir.join("state.json")
}
fn log_path(app_dir: &Path) -> PathBuf {
    app_dir.join("logs").join("app.log")
}
fn current_link(app_dir: &Path) -> PathBuf {
    app_dir.join("current")
}

fn switch_current(app_dir: &Path, slot: Slot) -> Result<()> {
    fsutil::symlink_atomic(
        &Path::new("slots").join(slot.as_str()),
        &current_link(app_dir),
    )
}

fn read_current(app_dir: &Path) -> Option<Slot> {
    let target = fs::read_link(current_link(app_dir)).ok()?;
    match target.file_name()?.to_str()? {
        "a" => Some(Slot::A),
        "b" => Some(Slot::B),
        _ => None,
    }
}

fn read_meta(app_dir: &Path, slot: Slot) -> Result<SlotMeta> {
    Ok(serde_json::from_slice(&fs::read(meta_path(
        app_dir, slot,
    ))?)?)
}

fn write_meta(app_dir: &Path, slot: Slot, meta: &SlotMeta) -> Result<()> {
    fsutil::write_atomic(&meta_path(app_dir, slot), &serde_json::to_vec_pretty(meta)?)
}

/// Removes a staging directory when dropped, whatever the outcome.
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Manager {
    /// Open (or create) the state directory and load installed apps,
    /// recovering any update that was interrupted by a crash or power loss.
    pub fn open(cfg: ManagerConfig) -> Result<Arc<Manager>> {
        let apps_dir = cfg.root.join("apps");
        fs::create_dir_all(&apps_dir)?;
        let staging = cfg.root.join("staging");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;

        let metrics = Arc::new(Metrics::new());
        let mut apps = BTreeMap::new();
        let mut dirs: Vec<PathBuf> = fs::read_dir(&apps_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            match load_app(&dir, &metrics) {
                Ok(app) => {
                    metrics.set_info(
                        &app.state.name,
                        None,
                        (&app.state.active.version, app.state.active.slot.as_str()),
                    );
                    metrics.app_up.with_label_values(&[&app.state.name]).set(0);
                    apps.insert(app.state.name.clone(), Arc::new(Mutex::new(app)));
                }
                Err(e) => error!("skipping {}: {e}", dir.display()),
            }
        }
        info!("loaded {} app(s) from {}", apps.len(), cfg.root.display());
        Ok(Arc::new(Manager {
            cfg,
            metrics,
            apps: RwLock::new(apps),
            staging_seq: AtomicU64::new(0),
        }))
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    fn app_dir(&self, name: &str) -> PathBuf {
        self.cfg.root.join("apps").join(name)
    }

    fn get(&self, name: &str) -> Result<Arc<Mutex<App>>> {
        self.apps
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| EdgeError::NotFound(name.to_string()))
    }

    fn all(&self) -> Vec<Arc<Mutex<App>>> {
        self.apps.read().unwrap().values().cloned().collect()
    }

    fn save(&self, app: &App) -> Result<()> {
        fsutil::write_atomic(
            &state_path(&app.dir),
            &serde_json::to_vec_pretty(&app.state)?,
        )
    }

    /// Download (if remote), unpack and verify an artifact in a private
    /// staging directory. Nothing outside staging is touched.
    fn stage_verified(&self, source: &str) -> Result<(Staging, VerifiedImage)> {
        let seq = self.staging_seq.fetch_add(1, Ordering::Relaxed);
        let staging = Staging(self.cfg.root.join("staging").join(format!("s{seq}")));
        let layout = fetch::stage(source, &staging.0, &self.cfg.fetch)?;
        match oci::verify_layout(&layout) {
            Ok(img) => {
                self.metrics.verifications.with_label_values(&["ok"]).inc();
                Ok((staging, img))
            }
            Err(e) => {
                self.metrics
                    .verifications
                    .with_label_values(&["failed"])
                    .inc();
                warn!("rejected artifact {source}: {e}");
                Err(e)
            }
        }
    }

    fn spawn_app(&self, app: &mut App) -> Result<()> {
        let workdir = slot_dir(&app.dir, app.state.active.slot);
        let handle = process::spawn(&app.spec, &workdir, &log_path(&app.dir))?;
        app.cgroup = match self
            .cfg
            .cgroups
            .apply(&app.spec.name, &app.spec.resources, handle.pid)
        {
            Ok(path) => path,
            Err(e) => {
                warn!("{}: could not apply cgroup limits: {e}", app.spec.name);
                None
            }
        };
        info!(
            "{}: started v{} from slot {} (pid {})",
            app.spec.name,
            app.spec.version,
            app.state.active.slot.as_str(),
            handle.pid
        );
        app.proc = Some(handle);
        app.health = Health::Starting;
        app.last_probe = None;
        app.probe_failures = 0;
        self.metrics
            .app_up
            .with_label_values(&[&app.spec.name])
            .set(1);
        Ok(())
    }

    fn stop_proc(&self, app: &mut App) {
        if let Some(mut p) = app.proc.take() {
            p.stop(self.cfg.stop_grace);
            info!("{}: stopped (pid {})", app.spec.name, p.pid);
        }
        self.metrics
            .app_up
            .with_label_values(&[&app.spec.name])
            .set(0);
        self.metrics
            .app_healthy
            .with_label_values(&[&app.spec.name])
            .set(0);
    }

    fn probe(&self, app: &mut App) -> ProbeResult {
        let alive = app.proc.as_mut().is_some_and(|p| p.alive());
        let workdir = slot_dir(&app.dir, app.state.active.slot);
        let t = Instant::now();
        let r = health::probe(&app.spec.health, alive, &workdir);
        self.metrics
            .health_duration
            .with_label_values(&[&app.spec.name])
            .observe(t.elapsed().as_secs_f64());
        self.metrics
            .app_healthy
            .with_label_values(&[&app.spec.name])
            .set(r.is_healthy() as i64);
        app.last_probe = Some(Instant::now());
        r
    }

    /// Health gate for a freshly started version.
    fn wait_healthy(&self, app: &mut App) -> std::result::Result<(), String> {
        let h = app.spec.health.clone();
        let deadline = Instant::now() + Duration::from_millis(h.grace_ms);
        let mut passed = 0;
        let mut last = "no probe passed".to_string();
        loop {
            if let Some(p) = app.proc.as_mut() {
                if let Some(status) = p.exit_status() {
                    return Err(format!(
                        "v{} exited during health gate ({status})",
                        app.spec.version
                    ));
                }
            }
            match self.probe(app) {
                ProbeResult::Healthy => {
                    passed += 1;
                    if passed >= h.success_threshold {
                        app.health = Health::Healthy;
                        return Ok(());
                    }
                }
                ProbeResult::Unhealthy(why) => {
                    passed = 0;
                    last = why;
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "v{} not healthy after {}ms: {last}",
                    app.spec.version, h.grace_ms
                ));
            }
            thread::sleep(Duration::from_millis(h.interval_ms));
        }
    }

    // ---------------------------------------------------------------- operations

    /// Install an artifact into slot A. The app is not started unless `start`.
    pub fn install(&self, source: &str, start: bool) -> Result<AppStatus> {
        let r = self.install_inner(source, start);
        self.metrics.op("install", r.is_ok());
        r
    }

    fn install_inner(&self, source: &str, start: bool) -> Result<AppStatus> {
        let (_staging, img) = self.stage_verified(source)?;
        let name = img.spec.name.clone();
        let dir = self.app_dir(&name);
        if self.apps.read().unwrap().contains_key(&name) || dir.exists() {
            return Err(EdgeError::AlreadyExists(name));
        }

        let build = || -> Result<App> {
            let slot = slot_dir(&dir, Slot::A);
            fsutil::reset_dir(&slot)?;
            fs::create_dir_all(slot_dir(&dir, Slot::B))?;
            let files = oci::unpack_layers(&img, &slot)?;
            write_meta(
                &dir,
                Slot::A,
                &SlotMeta {
                    spec: img.spec.clone(),
                    manifest_digest: img.manifest_digest.clone(),
                },
            )?;
            fs::create_dir_all(dir.join("logs"))?;
            switch_current(&dir, Slot::A)?;
            let t = now();
            let app = App {
                dir: dir.clone(),
                state: AppState {
                    name: name.clone(),
                    active: SlotRef {
                        slot: Slot::A,
                        version: img.spec.version.clone(),
                    },
                    previous: None,
                    pending: None,
                    desired_running: false,
                    installed_at: t,
                    updated_at: t,
                },
                spec: img.spec.clone(),
                manifest_digest: img.manifest_digest.clone(),
                proc: None,
                health: Health::Stopped,
                restarts: 0,
                crashes: 0,
                next_restart_at: None,
                last_probe: None,
                probe_failures: 0,
                cgroup: None,
                last_error: None,
            };
            self.save(&app)?;
            info!(
                "{name}: installed v{} ({files} files, {})",
                img.spec.version, img.manifest_digest
            );
            Ok(app)
        };
        let app = match build() {
            Ok(a) => a,
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(e);
            }
        };

        {
            let mut apps = self.apps.write().unwrap();
            if apps.contains_key(&name) {
                return Err(EdgeError::AlreadyExists(name));
            }
            apps.insert(name.clone(), Arc::new(Mutex::new(app)));
        }
        self.metrics
            .set_info(&name, None, (&img.spec.version, Slot::A.as_str()));
        self.metrics.app_up.with_label_values(&[&name]).set(0);

        if start {
            return self.start(&name);
        }
        self.status(&name)
    }

    pub fn start(&self, name: &str) -> Result<AppStatus> {
        let r = (|| {
            let arc = self.get(name)?;
            let mut app = arc.lock().unwrap();
            if !app.proc.as_mut().is_some_and(|p| p.alive()) {
                app.crashes = 0;
                app.next_restart_at = None;
                self.spawn_app(&mut app)?;
            }
            app.state.desired_running = true;
            self.save(&app)?;
            Ok(self.status_of(&mut app))
        })();
        self.metrics.op("start", r.is_ok());
        r
    }

    pub fn stop(&self, name: &str) -> Result<AppStatus> {
        let r = (|| {
            let arc = self.get(name)?;
            let mut app = arc.lock().unwrap();
            app.state.desired_running = false;
            self.stop_proc(&mut app);
            app.health = Health::Stopped;
            self.save(&app)?;
            Ok(self.status_of(&mut app))
        })();
        self.metrics.op("stop", r.is_ok());
        r
    }

    pub fn restart(&self, name: &str) -> Result<AppStatus> {
        let r = (|| {
            let arc = self.get(name)?;
            let mut app = arc.lock().unwrap();
            self.stop_proc(&mut app);
            app.crashes = 0;
            app.next_restart_at = None;
            self.spawn_app(&mut app)?;
            app.state.desired_running = true;
            self.save(&app)?;
            Ok(self.status_of(&mut app))
        })();
        self.metrics.op("restart", r.is_ok());
        r
    }

    /// OTA update with A/B slots, health gate and automatic rollback.
    pub fn update(&self, name: &str, source: &str) -> Result<AppStatus> {
        let r = self.update_inner(name, source);
        self.metrics.op("update", r.is_ok());
        let result = match &r {
            Ok(_) => "success",
            Err(EdgeError::RolledBack(_)) => "rolled_back",
            Err(EdgeError::Fetch(_)) => "fetch_failed",
            Err(EdgeError::Verify(_)) => "rejected",
            Err(_) => "error",
        };
        self.metrics
            .updates
            .with_label_values(&[name, result])
            .inc();
        r
    }

    fn update_inner(&self, name: &str, source: &str) -> Result<AppStatus> {
        let arc = self.get(name)?;
        let mut app = arc.lock().unwrap();

        // 1. Fetch + verify. A bad artifact or a dropped connection stops here
        //    with the running version untouched.
        let (_staging, img) = self.stage_verified(source).inspect_err(|e| {
            app.last_error = Some(e.to_string());
        })?;
        if img.spec.name != name {
            return Err(EdgeError::Invalid(format!(
                "artifact is for app '{}', not '{name}'",
                img.spec.name
            )));
        }

        // 2. Write the new version into the inactive slot. That slot may hold
        //    the previous version; once we start overwriting it, it can no
        //    longer be rolled back to, so drop it from the state first.
        let from = app.state.active.clone();
        let target = from.slot.other();
        if app
            .state
            .previous
            .as_ref()
            .is_some_and(|p| p.slot == target)
        {
            app.state.previous = None;
            self.save(&app)?;
        }
        let tdir = slot_dir(&app.dir, target);
        fsutil::reset_dir(&tdir)?;
        oci::unpack_layers(&img, &tdir)?;
        write_meta(
            &app.dir,
            target,
            &SlotMeta {
                spec: img.spec.clone(),
                manifest_digest: img.manifest_digest.clone(),
            },
        )?;
        let to = SlotRef {
            slot: target,
            version: img.spec.version.clone(),
        };

        // 3. Record the transaction before touching `current`.
        app.state.pending = Some(PendingUpdate {
            from: from.clone(),
            to: to.clone(),
            started_at: now(),
        });
        self.save(&app)?;
        info!(
            "{name}: updating v{} (slot {}) -> v{} (slot {})",
            from.version,
            from.slot.as_str(),
            to.version,
            to.slot.as_str()
        );

        // 4. Swap and start the new version behind the health gate.
        let was_running = app.state.desired_running;
        self.stop_proc(&mut app);
        switch_current(&app.dir, target)?;
        let old_spec = std::mem::replace(&mut app.spec, img.spec.clone());
        let old_digest = std::mem::replace(&mut app.manifest_digest, img.manifest_digest.clone());
        app.state.active = to.clone();

        if was_running {
            let gate = self
                .spawn_app(&mut app)
                .map_err(|e| e.to_string())
                .and_then(|_| self.wait_healthy(&mut app));
            if let Err(reason) = gate {
                warn!("{name}: {reason}; rolling back to v{}", from.version);
                self.stop_proc(&mut app);
                switch_current(&app.dir, from.slot)?;
                app.spec = old_spec;
                app.manifest_digest = old_digest;
                app.state.active = from.clone();
                app.state.pending = None;
                self.save(&app)?;
                if let Err(e) = self.spawn_app(&mut app) {
                    error!(
                        "{name}: failed to restart v{} after rollback: {e}",
                        from.version
                    );
                }
                app.last_error = Some(reason.clone());
                self.metrics
                    .rollbacks
                    .with_label_values(&[name, "health_gate"])
                    .inc();
                return Err(EdgeError::RolledBack(reason));
            }
        }

        // 5. Commit.
        app.state.previous = Some(from.clone());
        app.state.pending = None;
        app.state.updated_at = now();
        app.crashes = 0;
        app.last_error = None;
        self.save(&app)?;
        self.metrics.set_info(
            name,
            Some((&from.version, from.slot.as_str())),
            (&to.version, to.slot.as_str()),
        );
        info!("{name}: update to v{} committed", to.version);
        Ok(self.status_of(&mut app))
    }

    /// Manually switch back to the previous slot.
    pub fn rollback(&self, name: &str) -> Result<AppStatus> {
        let r = (|| {
            let arc = self.get(name)?;
            let mut app = arc.lock().unwrap();
            let prev = app
                .state
                .previous
                .clone()
                .ok_or_else(|| EdgeError::Invalid(format!("{name} has no previous version")))?;
            let meta = read_meta(&app.dir, prev.slot)?;
            let was_running = app.state.desired_running;
            self.stop_proc(&mut app);
            switch_current(&app.dir, prev.slot)?;
            let cur = std::mem::replace(&mut app.state.active, prev.clone());
            app.state.previous = Some(cur.clone());
            app.state.updated_at = now();
            app.spec = meta.spec;
            app.manifest_digest = meta.manifest_digest;
            app.crashes = 0;
            self.save(&app)?;
            if was_running {
                self.spawn_app(&mut app)?;
            }
            self.metrics
                .rollbacks
                .with_label_values(&[name, "manual"])
                .inc();
            self.metrics.set_info(
                name,
                Some((&cur.version, cur.slot.as_str())),
                (&prev.version, prev.slot.as_str()),
            );
            info!("{name}: rolled back v{} -> v{}", cur.version, prev.version);
            Ok(self.status_of(&mut app))
        })();
        self.metrics.op("rollback", r.is_ok());
        r
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let r = (|| {
            let arc = self
                .apps
                .write()
                .unwrap()
                .remove(name)
                .ok_or_else(|| EdgeError::NotFound(name.to_string()))?;
            let mut app = arc.lock().unwrap();
            self.stop_proc(&mut app);
            self.cfg.cgroups.remove(name);
            fs::remove_dir_all(&app.dir)?;
            self.metrics.forget_app(
                name,
                &app.state.active.version,
                app.state.active.slot.as_str(),
            );
            info!("{name}: removed");
            Ok(())
        })();
        self.metrics.op("remove", r.is_ok());
        r
    }

    pub fn status(&self, name: &str) -> Result<AppStatus> {
        let arc = self.get(name)?;
        let mut app = arc.lock().unwrap();
        Ok(self.status_of(&mut app))
    }

    /// Status of every app. Apps busy in an update are reported as `busy`
    /// instead of blocking the caller.
    pub fn list(&self) -> Vec<AppStatus> {
        let mut out = Vec::new();
        for (name, arc) in self.apps.read().unwrap().clone() {
            match arc.try_lock() {
                Ok(mut app) => out.push(self.status_of(&mut app)),
                Err(_) => out.push(AppStatus {
                    name,
                    version: String::new(),
                    slot: None,
                    previous_version: None,
                    manifest_digest: None,
                    desired_running: true,
                    running: true,
                    pid: None,
                    health: Health::Busy,
                    restarts: 0,
                    memory_bytes: None,
                    cpu_seconds: None,
                    cgroup: None,
                    last_error: None,
                    updated_at: 0,
                }),
            }
        }
        out
    }

    fn status_of(&self, app: &mut App) -> AppStatus {
        let running = app.proc.as_mut().is_some_and(|p| p.alive());
        let pid = if running {
            app.proc.as_ref().map(|p| p.pid)
        } else {
            None
        };
        let (memory_bytes, cpu_seconds) = self.resource_usage(app, pid);
        AppStatus {
            name: app.state.name.clone(),
            version: app.state.active.version.clone(),
            slot: Some(app.state.active.slot),
            previous_version: app.state.previous.as_ref().map(|p| p.version.clone()),
            manifest_digest: Some(app.manifest_digest.clone()),
            desired_running: app.state.desired_running,
            running,
            pid,
            health: app.health,
            restarts: app.restarts,
            memory_bytes,
            cpu_seconds,
            cgroup: app.cgroup.as_ref().map(|p| p.display().to_string()),
            last_error: app.last_error.clone(),
            updated_at: app.state.updated_at,
        }
    }

    fn resource_usage(&self, app: &App, pid: Option<u32>) -> (Option<u64>, Option<f64>) {
        let pid = match pid {
            Some(p) => p,
            None => return (None, None),
        };
        if app.cgroup.is_some() {
            if let Some(s) = self.cfg.cgroups.stats(&app.spec.name) {
                return (Some(s.memory_bytes), Some(s.cpu_seconds));
            }
        }
        match process::proc_stats(pid) {
            Some((m, c)) => (Some(m), Some(c)),
            None => (None, None),
        }
    }

    // ---------------------------------------------------------------- supervision

    /// Start every app whose desired state is running (used at boot).
    pub fn start_desired(&self) {
        for arc in self.all() {
            let mut app = arc.lock().unwrap();
            if app.state.desired_running && app.proc.is_none() {
                if let Err(e) = self.spawn_app(&mut app) {
                    error!("{}: failed to start at boot: {e}", app.state.name);
                    app.last_error = Some(e.to_string());
                }
            }
        }
    }

    /// One supervision pass: restart crashed apps with exponential backoff,
    /// run due health probes and refresh resource metrics.
    pub fn supervise_tick(&self) {
        for arc in self.all() {
            let Ok(mut app) = arc.try_lock() else {
                continue;
            };
            if app.state.desired_running && app.health != Health::CrashLoop {
                self.supervise_app(&mut app);
            }
            let pid = app
                .proc
                .as_mut()
                .and_then(|p| if p.alive() { Some(p.pid) } else { None });
            let (mem, cpu) = self.resource_usage(&app, pid);
            let name = app.spec.name.clone();
            if let Some(m) = mem {
                self.metrics
                    .app_memory
                    .with_label_values(&[&name])
                    .set(m as f64);
            }
            if let Some(c) = cpu {
                self.metrics.app_cpu.with_label_values(&[&name]).set(c);
            }
        }
    }

    fn supervise_app(&self, app: &mut App) {
        let name = app.spec.name.clone();
        let alive = app.proc.as_mut().is_some_and(|p| p.alive());

        if alive {
            let due = app
                .last_probe
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(app.spec.health.interval_ms));
            if !due {
                return;
            }
            match self.probe(app) {
                ProbeResult::Healthy => {
                    app.health = Health::Healthy;
                    app.probe_failures = 0;
                    let stable = Duration::from_millis(app.spec.restart.stable_ms);
                    if app.proc.as_ref().is_some_and(|p| p.uptime() >= stable) {
                        app.crashes = 0;
                    }
                }
                ProbeResult::Unhealthy(why) => {
                    app.probe_failures += 1;
                    if app.probe_failures >= app.spec.health.failure_threshold {
                        warn!(
                            "{name}: {} failed probes ({why}), restarting",
                            app.probe_failures
                        );
                        app.health = Health::Unhealthy;
                        app.last_error = Some(why);
                        self.stop_proc(app);
                        self.record_crash(app);
                    }
                }
            }
            return;
        }

        if let Some(mut p) = app.proc.take() {
            let status = p
                .exit_status()
                .map_or_else(|| "unknown status".into(), |s| s.to_string());
            warn!("{name}: process {} exited ({status})", p.pid);
            app.last_error = Some(format!("process exited ({status})"));
            self.metrics.app_up.with_label_values(&[&name]).set(0);
            self.metrics.app_healthy.with_label_values(&[&name]).set(0);
            self.record_crash(app);
        }
        if app.health == Health::CrashLoop {
            return;
        }
        let due = app.next_restart_at.is_none_or(|t| Instant::now() >= t);
        if due {
            match self.spawn_app(app) {
                Ok(()) => {
                    app.restarts += 1;
                    self.metrics.app_restarts.with_label_values(&[&name]).inc();
                }
                Err(e) => {
                    app.last_error = Some(e.to_string());
                    self.record_crash(app);
                }
            }
        }
    }

    fn record_crash(&self, app: &mut App) {
        app.crashes += 1;
        let r = &app.spec.restart;
        if app.crashes > r.max_restarts {
            error!(
                "{}: crashed {} times, giving up (crash loop)",
                app.spec.name, app.crashes
            );
            app.health = Health::CrashLoop;
            app.next_restart_at = None;
            return;
        }
        let exp = (app.crashes - 1).min(10);
        let delay = Duration::from_millis(r.backoff_ms.saturating_mul(1 << exp))
            .min(Duration::from_secs(300));
        app.next_restart_at = Some(Instant::now() + delay);
        app.health = Health::Unhealthy;
    }

    /// Run [`Manager::supervise_tick`] forever on a background thread.
    pub fn spawn_supervisor(self: &Arc<Self>, tick: Duration) -> thread::JoinHandle<()> {
        let mgr = Arc::clone(self);
        thread::Builder::new()
            .name("supervisor".into())
            .spawn(move || loop {
                mgr.supervise_tick();
                thread::sleep(tick);
            })
            .expect("spawn supervisor thread")
    }

    /// Stop all processes but keep their desired state, so they come back
    /// when the daemon restarts.
    pub fn shutdown(&self) {
        for arc in self.all() {
            let mut app = arc.lock().unwrap();
            self.stop_proc(&mut app);
        }
    }
}

fn load_app(dir: &Path, metrics: &Metrics) -> Result<App> {
    let tmp = fsutil::tmp_link_path(&current_link(dir));
    if fs::symlink_metadata(&tmp).is_ok() {
        warn!("{}: removing stale {}", dir.display(), tmp.display());
        fs::remove_file(&tmp)?;
    }

    let mut state: AppState = serde_json::from_slice(&fs::read(state_path(dir))?)?;
    if let Some(p) = state.pending.take() {
        warn!(
            "{}: found interrupted update v{} -> v{}; restoring v{} (slot {})",
            state.name,
            p.from.version,
            p.to.version,
            p.from.version,
            p.from.slot.as_str()
        );
        state.active = p.from;
        metrics
            .rollbacks
            .with_label_values(&[&state.name, "interrupted_update"])
            .inc();
    }
    if read_current(dir) != Some(state.active.slot) {
        switch_current(dir, state.active.slot)?;
    }
    fsutil::write_atomic(&state_path(dir), &serde_json::to_vec_pretty(&state)?)?;

    let meta = read_meta(dir, state.active.slot)?;
    Ok(App {
        dir: dir.to_path_buf(),
        health: if state.desired_running {
            Health::Unknown
        } else {
            Health::Stopped
        },
        state,
        spec: meta.spec,
        manifest_digest: meta.manifest_digest,
        proc: None,
        restarts: 0,
        crashes: 0,
        next_restart_at: None,
        last_probe: None,
        probe_failures: 0,
        cgroup: None,
        last_error: None,
    })
}
