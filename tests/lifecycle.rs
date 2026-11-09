//! Happy-path lifecycle, cgroups, metrics and HTTP API tests.

mod common;

use std::fs;
use std::time::Duration;

use common::*;
use edgeforge::api::ApiServer;
use edgeforge::{EdgeError, Health, Slot};

#[test]
fn full_lifecycle_install_start_update_rollback_stop_remove() {
    let env = env();
    let v1 = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    let v2 = artifact(&env, &spec("detector", "1.1.0"), SERVE);

    // install + start
    let st = env.mgr.install(&s(&v1), true).unwrap();
    assert_eq!(
        (st.version.as_str(), st.slot, st.running),
        ("1.0.0", Some(Slot::A), true)
    );
    assert!(env.tick_until(Duration::from_secs(3), |m| m
        .status("detector")
        .unwrap()
        .health
        == Health::Healthy));
    let log = env.app_dir("detector").join("logs/app.log");
    assert!(
        env.tick_until(Duration::from_secs(3), |_| fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("serving detector v1.0.0"))
    );

    // OTA update A -> B
    let st = env.mgr.update("detector", &s(&v2)).unwrap();
    assert_eq!((st.version.as_str(), st.slot), ("1.1.0", Some(Slot::B)));
    assert_eq!(st.previous_version.as_deref(), Some("1.0.0"));
    assert!(st.running);
    assert_eq!(
        fs::read_to_string(env.app_dir("detector").join("current/model/labels.txt")).unwrap(),
        "person\ncar\n# 1.1.0\n"
    );

    // manual rollback B -> A
    let st = env.mgr.rollback("detector").unwrap();
    assert_eq!((st.version.as_str(), st.slot), ("1.0.0", Some(Slot::A)));
    assert_eq!(st.previous_version.as_deref(), Some("1.1.0"));

    // stop / start / restart
    assert!(!env.mgr.stop("detector").unwrap().running);
    assert!(env.mgr.start("detector").unwrap().running);
    let pid1 = env.mgr.status("detector").unwrap().pid;
    let pid2 = env.mgr.restart("detector").unwrap().pid;
    assert_ne!(pid1, pid2);

    // remove
    env.mgr.remove("detector").unwrap();
    assert!(!env.app_dir("detector").exists());
    assert!(matches!(
        env.mgr.status("detector"),
        Err(EdgeError::NotFound(_))
    ));
    assert_eq!(
        metric(
            &env.mgr,
            "edgeforge_lifecycle_operations_total{op=\"remove\",result=\"ok\"}"
        ),
        1.0
    );
}

#[test]
fn duplicate_install_is_rejected() {
    let env = env();
    let v1 = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    env.mgr.install(&s(&v1), false).unwrap();
    assert!(matches!(
        env.mgr.install(&s(&v1), false),
        Err(EdgeError::AlreadyExists(_))
    ));
}

#[test]
fn update_from_http_server() {
    let env = env();
    let v1 = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    env.mgr.install(&s(&v1), true).unwrap();
    let v2 = artifact(&env, &spec("detector", "2.0.0"), SERVE);
    let url = serve(fs::read(&v2).unwrap(), Fault::None);

    let st = env.mgr.update("detector", &url).unwrap();
    assert_eq!(st.version, "2.0.0");
}

#[test]
fn cgroup_limits_are_written_before_the_app_runs_long() {
    let env = env();
    let mut sp = spec("detector", "1.0.0");
    sp.resources.memory_max_bytes = Some(256 * 1024 * 1024);
    sp.resources.cpu_max_millicores = Some(750);
    let a = artifact(&env, &sp, SERVE);
    let st = env.mgr.install(&s(&a), true).unwrap();

    let cg = env.cgroup_root().join("detector");
    assert_eq!(
        fs::read_to_string(cg.join("memory.max")).unwrap(),
        "268435456"
    );
    assert_eq!(
        fs::read_to_string(cg.join("cpu.max")).unwrap(),
        "75000 100000"
    );
    assert_eq!(
        fs::read_to_string(cg.join("cgroup.procs")).unwrap(),
        st.pid.unwrap().to_string()
    );

    // Usage comes from the cgroup files when present.
    fs::write(cg.join("memory.current"), "41943040").unwrap();
    fs::write(cg.join("cpu.stat"), "usage_usec 1250000\n").unwrap();
    let st = env.mgr.status("detector").unwrap();
    assert_eq!(st.memory_bytes, Some(41_943_040));
    assert_eq!(st.cpu_seconds, Some(1.25));

    env.mgr.remove("detector").unwrap();
    assert!(!cg.exists());
}

#[test]
fn prometheus_metrics_reflect_state() {
    let env = env();
    let a = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    env.mgr.install(&s(&a), true).unwrap();
    assert!(env.tick_until(Duration::from_secs(3), |m| m
        .status("detector")
        .unwrap()
        .health
        == Health::Healthy));
    env.mgr.supervise_tick();

    let text = env.mgr.metrics().render();
    for line in [
        "edgeforge_app_up{app=\"detector\"} 1",
        "edgeforge_app_healthy{app=\"detector\"} 1",
        "edgeforge_app_info{app=\"detector\",slot=\"a\",version=\"1.0.0\"} 1",
        "edgeforge_artifact_verifications_total{result=\"ok\"} 1",
        "edgeforge_lifecycle_operations_total{op=\"install\",result=\"ok\"} 1",
    ] {
        assert!(text.contains(line), "missing `{line}` in:\n{text}");
    }
    assert!(text.contains("edgeforge_health_probe_duration_seconds_bucket"));
    assert!(metric(&env.mgr, "edgeforge_app_memory_bytes{app=\"detector\"}") > 0.0);
}

#[test]
fn daemon_restart_brings_desired_apps_back() {
    let mut env = env();
    let a = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    env.mgr.install(&s(&a), true).unwrap();
    let b = artifact(&env, &spec("idle", "1.0.0"), SERVE);
    env.mgr.install(&s(&b), false).unwrap();

    env.reopen();
    env.mgr.start_desired();

    assert!(env.mgr.status("detector").unwrap().running);
    assert!(!env.mgr.status("idle").unwrap().running);
}

#[test]
fn unhealthy_running_app_is_restarted() {
    let env = env();
    let mut sp = spec("detector", "1.0.0");
    // Healthy until the app deletes its own marker file.
    sp.health.probe = edgeforge::spec::HealthProbe::Exec {
        command: vec!["test".into(), "-f".into(), "ready".into()],
    };
    let script =
        "#!/bin/sh\ntouch ready\nsleep 0.4\nrm -f ready\nwhile true; do sleep 0.05; done\n";
    let a = artifact(&env, &sp, script);
    env.mgr.install(&s(&a), true).unwrap();

    assert!(env.tick_until(Duration::from_secs(5), |m| m
        .status("detector")
        .unwrap()
        .restarts
        >= 1));
}

#[test]
fn http_api_end_to_end() {
    let env = env();
    let api = ApiServer::start(env.mgr.clone(), "127.0.0.1:0", 2).unwrap();
    let base = format!("http://{}", api.addr);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let a = artifact(&env, &spec("detector", "1.0.0"), SERVE);

    let mut r = agent
        .post(&format!("{base}/api/apps"))
        .header("Content-Type", "application/json")
        .send(serde_json::json!({ "source": s(&a), "start": true }).to_string())
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let body: serde_json::Value =
        serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap();
    assert_eq!(body["version"], "1.0.0");

    let mut r = agent.get(&format!("{base}/api/apps")).call().unwrap();
    let list: serde_json::Value =
        serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap();
    assert_eq!(list[0]["name"], "detector");

    let mut r = agent.get(&format!("{base}/metrics")).call().unwrap();
    assert!(r
        .body_mut()
        .read_to_string()
        .unwrap()
        .contains("edgeforge_app_up"));

    let mut r = agent.get(&format!("{base}/")).call().unwrap();
    assert!(r
        .body_mut()
        .read_to_string()
        .unwrap()
        .contains("<title>EdgeForge</title>"));

    let r = agent
        .post(&format!("{base}/api/apps/nope/start"))
        .send_empty()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);

    let r = agent
        .post(&format!("{base}/api/apps"))
        .header("Content-Type", "application/json")
        .send("{not json")
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);

    let r = agent
        .delete(&format!("{base}/api/apps/detector"))
        .call()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    api.shutdown();
}
