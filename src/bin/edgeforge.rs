//! `edgeforge` — CLI for packaging artifacts and driving the daemon.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use edgeforge::{fetch, oci, pack};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(version, about = "EdgeForge CLI")]
struct Cli {
    /// Daemon API address.
    #[arg(
        long,
        global = true,
        default_value = "http://127.0.0.1:9100",
        env = "EDGEFORGE_API"
    )]
    api: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Package an app directory (containing edgeforge.json) into an OCI artifact.
    Pack {
        dir: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Override the version from edgeforge.json.
        #[arg(long)]
        version: Option<String>,
    },
    /// Verify an artifact offline and print its manifest summary.
    Verify {
        artifact: String,
    },
    /// Install an artifact (path or URL).
    Install {
        source: String,
        #[arg(long)]
        start: bool,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    /// OTA-update an app; rolled back automatically if the new version is unhealthy.
    Update {
        name: String,
        source: String,
    },
    /// Switch back to the previous version.
    Rollback {
        name: String,
    },
    Remove {
        name: String,
    },
    /// Show all apps.
    List,
    Status {
        name: String,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let api = cli.api.trim_end_matches('/').to_string();
    match cli.cmd {
        Cmd::Pack { dir, out, version } => {
            let mut spec = pack::load_spec(&dir)?;
            if let Some(v) = version {
                spec.version = v;
            }
            let layout = pack::pack_app(&dir, &spec, &out)?;
            println!(
                "packed {} v{} -> {}",
                spec.name,
                spec.version,
                out.display()
            );
            println!("manifest {}", layout.manifest_digest);
        }
        Cmd::Verify { artifact } => {
            let tmp = std::env::temp_dir().join(format!("edgeforge-verify-{}", std::process::id()));
            let r = fetch::stage(&artifact, &tmp, &fetch::FetchOptions::default())
                .and_then(|layout| oci::verify_layout(&layout));
            let _ = std::fs::remove_dir_all(&tmp);
            let img = r?;
            println!("OK  {} v{}", img.spec.name, img.spec.version);
            println!("    manifest {}", img.manifest_digest);
            println!("    {} layer(s) verified", img.layers.len());
        }
        Cmd::Install { source, start } => print(call(
            "POST",
            &format!("{api}/api/apps"),
            Some(json!({ "source": absolute(&source), "start": start })),
        )?),
        Cmd::Update { name, source } => print(call(
            "POST",
            &format!("{api}/api/apps/{name}/update"),
            Some(json!({ "source": absolute(&source) })),
        )?),
        Cmd::Start { name } => print(action(&api, &name, "start")?),
        Cmd::Stop { name } => print(action(&api, &name, "stop")?),
        Cmd::Restart { name } => print(action(&api, &name, "restart")?),
        Cmd::Rollback { name } => print(action(&api, &name, "rollback")?),
        Cmd::Remove { name } => print(call("DELETE", &format!("{api}/api/apps/{name}"), None)?),
        Cmd::Status { name } => print(call("GET", &format!("{api}/api/apps/{name}"), None)?),
        Cmd::List => list(call("GET", &format!("{api}/api/apps"), None)?),
    }
    Ok(())
}

/// The daemon resolves paths on its own filesystem, so send absolute paths.
fn absolute(source: &str) -> String {
    if source.contains("://") {
        return source.to_string();
    }
    std::fs::canonicalize(Path::new(source))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| source.to_string())
}

fn action(api: &str, name: &str, act: &str) -> anyhow::Result<Value> {
    call("POST", &format!("{api}/api/apps/{name}/{act}"), None)
}

fn call(method: &str, url: &str, body: Option<Value>) -> anyhow::Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = match (method, body) {
        ("GET", _) => agent.get(url).call(),
        ("DELETE", _) => agent.delete(url).call(),
        (_, Some(b)) => agent
            .post(url)
            .header("Content-Type", "application/json")
            .send(b.to_string()),
        (_, None) => agent.post(url).send_empty(),
    }
    .with_context(|| format!("cannot reach edgeforged at {url}"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string()?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    if status >= 400 {
        let msg = value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        bail!("{msg} (HTTP {status})");
    }
    Ok(value)
}

fn print(v: Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

fn list(v: Value) {
    println!(
        "{:<20} {:<10} {:<5} {:<11} {:>7} {:>9} {:>8}",
        "NAME", "VERSION", "SLOT", "HEALTH", "PID", "MEM(MiB)", "RESTARTS"
    );
    for a in v.as_array().into_iter().flatten() {
        let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("-").to_string();
        let pid = a
            .get("pid")
            .and_then(Value::as_u64)
            .map_or("-".into(), |p| p.to_string());
        let mem = a
            .get("memory_bytes")
            .and_then(Value::as_u64)
            .map_or("-".into(), |m| format!("{:.1}", m as f64 / 1048576.0));
        println!(
            "{:<20} {:<10} {:<5} {:<11} {:>7} {:>9} {:>8}",
            s("name"),
            s("version"),
            s("slot"),
            s("health"),
            pid,
            mem,
            a.get("restarts").and_then(Value::as_u64).unwrap_or(0)
        );
    }
}
