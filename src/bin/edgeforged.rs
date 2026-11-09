//! `edgeforged` — the on-device daemon.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use edgeforge::api::ApiServer;
use edgeforge::cgroup::Cgroups;
use edgeforge::fetch::FetchOptions;
use edgeforge::{Manager, ManagerConfig};
use log::info;

#[derive(Parser)]
#[command(version, about = "EdgeForge on-device AI app lifecycle daemon")]
struct Args {
    /// State directory for slots, metadata and logs.
    #[arg(long, default_value = "/var/lib/edgeforge", env = "EDGEFORGE_ROOT")]
    root: PathBuf,
    /// Address for the control API, dashboard and /metrics.
    #[arg(long, default_value = "127.0.0.1:9100", env = "EDGEFORGE_LISTEN")]
    listen: String,
    /// Base cgroup for per-app limits (cgroups v2 unified hierarchy).
    #[arg(long, default_value = "/sys/fs/cgroup/edgeforge")]
    cgroup_root: PathBuf,
    /// Run without cgroups (resource usage then comes from /proc).
    #[arg(long)]
    no_cgroups: bool,
    /// Supervisor interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    tick_ms: u64,
    /// Seconds to wait after SIGTERM before SIGKILL when stopping apps.
    #[arg(long, default_value_t = 10)]
    stop_grace_secs: u64,
    /// Per-attempt download timeout in seconds.
    #[arg(long, default_value_t = 60)]
    fetch_timeout_secs: u64,
    /// Download retries after the first attempt.
    #[arg(long, default_value_t = 3)]
    fetch_retries: u32,
    #[arg(long, default_value_t = 4)]
    api_workers: usize,
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let cgroups = if args.no_cgroups {
        Cgroups::disabled()
    } else {
        Cgroups::detect(&args.cgroup_root)
    };
    let cfg = ManagerConfig {
        root: args.root.clone(),
        cgroups,
        fetch: FetchOptions {
            timeout: Duration::from_secs(args.fetch_timeout_secs),
            retries: args.fetch_retries,
            ..FetchOptions::default()
        },
        stop_grace: Duration::from_secs(args.stop_grace_secs),
    };
    let mgr = Manager::open(cfg).context("opening state directory")?;
    mgr.start_desired();
    mgr.spawn_supervisor(Duration::from_millis(args.tick_ms));
    let api = ApiServer::start(mgr.clone(), &args.listen, args.api_workers)?;
    info!(
        "edgeforged {} listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        api.addr
    );

    // SAFETY: installing a handler that only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    while !SHUTDOWN.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(200));
    }
    info!("shutting down: stopping apps");
    mgr.shutdown();
    api.shutdown();
    Ok(())
}
