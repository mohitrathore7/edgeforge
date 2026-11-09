//! Failure-injection tests. Each test breaks something an edge device sees in
//! the field — corrupted artifacts, crashing releases, flaky networks, power
//! loss — and checks that EdgeForge keeps the last known-good version.

mod common;

use std::fs;
use std::io::Write;
use std::time::{Duration, Instant};

use common::*;
use edgeforge::oci::{MT_CONFIG, MT_LAYER_GZIP};
use edgeforge::pack::{blob_key, LayoutFiles};
use edgeforge::spec::HealthProbe;
use edgeforge::{EdgeError, Health, Slot};
use flate2::write::GzEncoder;
use flate2::Compression;

/// Install and start v1 of `name`, waiting until it is healthy.
fn running_v1(env: &Env, name: &str) {
    let a = artifact(env, &spec(name, "1.0.0"), SERVE);
    env.mgr.install(&s(&a), true).unwrap();
    assert!(env.tick_until(Duration::from_secs(3), |m| {
        m.status(name).unwrap().health == Health::Healthy
    }));
}

fn assert_still_v1(env: &Env, name: &str) {
    let st = env.mgr.status(name).unwrap();
    assert_eq!(st.version, "1.0.0");
    assert_eq!(st.slot, Some(Slot::A));
    assert!(st.running, "v1 must keep running");
    assert_eq!(
        fs::read_link(env.app_dir(name).join("current")).unwrap(),
        std::path::PathBuf::from("slots/a")
    );
}

fn verify_err(r: edgeforge::Result<edgeforge::AppStatus>, needle: &str) {
    match r {
        Err(EdgeError::Verify(m)) => assert!(m.contains(needle), "unexpected message: {m}"),
        other => panic!("expected verification error containing '{needle}', got {other:?}"),
    }
}

// ---------------------------------------------------------- corrupted artifacts

#[test]
fn rejects_layer_with_flipped_byte() {
    let env = env();
    let mut l = layout(&env, &spec("detector", "1.0.0"), SERVE);
    let key = blob_key(&l.layer_digests[0]);
    let blob = l.files.get_mut(&key).unwrap();
    let mid = blob.len() / 2;
    blob[mid] ^= 0x01;
    let a = write(&env, &l, "corrupt.tar");

    verify_err(env.mgr.install(&s(&a), true), "digest mismatch");
    assert!(env.mgr.list().is_empty());
    assert!(!env.app_dir("detector").exists());
    assert_eq!(
        metric(
            &env.mgr,
            "edgeforge_artifact_verifications_total{result=\"failed\"}"
        ),
        1.0
    );
}

#[test]
fn rejects_truncated_layer_blob() {
    let env = env();
    let mut l = layout(&env, &spec("detector", "1.0.0"), SERVE);
    let key = blob_key(&l.layer_digests[0]);
    let blob = l.files.get_mut(&key).unwrap();
    blob.truncate(blob.len() - 7);
    let a = write(&env, &l, "truncated.tar");

    verify_err(env.mgr.install(&s(&a), false), "size mismatch");
}

#[test]
fn rejects_missing_config_blob() {
    let env = env();
    let mut l = layout(&env, &spec("detector", "1.0.0"), SERVE);
    l.files.remove(&blob_key(&l.config_digest));
    let a = write(&env, &l, "missing.tar");

    verify_err(env.mgr.install(&s(&a), false), "missing blob");
}

#[test]
fn rejects_manifest_edited_after_signing() {
    let env = env();
    let mut l = layout(&env, &spec("detector", "1.0.0"), SERVE);
    let key = blob_key(&l.manifest_digest);
    let text = String::from_utf8(l.files[&key].clone()).unwrap();
    // Point at a different (attacker controlled) version annotation without
    // updating the digest recorded in index.json.
    let edited = text.replace("1.0.0", "6.6.6");
    l.files.insert(key, edited.into_bytes());
    let a = write(&env, &l, "edited.tar");

    verify_err(env.mgr.install(&s(&a), false), "digest mismatch");
}

#[test]
fn rejects_unexpected_media_type() {
    let env = env();
    let sp = spec("detector", "1.0.0");
    let layer = edgeforge::pack::layer_from_dir(&app_source(&env, &sp, SERVE)).unwrap();
    // Correctly hashed, but the config is a generic container config.
    let l = LayoutFiles::build(
        &serde_json::to_vec(&sp).unwrap(),
        "application/vnd.oci.image.config.v1+json",
        &[(layer, MT_LAYER_GZIP)],
        "1.0.0",
    );
    let a = write(&env, &l, "mediatype.tar");

    verify_err(
        env.mgr.install(&s(&a), false),
        "unsupported config media type",
    );
}

fn evil_layer(name: &[u8], kind: tar::EntryType, link: Option<&str>) -> Vec<u8> {
    let mut b = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut h = tar::Header::new_old();
    h.as_old_mut().name[..name.len()].copy_from_slice(name);
    h.set_mode(0o755);
    h.set_entry_type(kind);
    let data: &[u8] = if kind == tar::EntryType::Regular {
        b"#!/bin/sh\necho pwned\n"
    } else {
        b""
    };
    h.set_size(data.len() as u64);
    if let Some(target) = link {
        h.set_link_name(target).unwrap();
    }
    h.set_cksum();
    b.append(&h, data).unwrap();
    b.into_inner().unwrap().finish().unwrap()
}

#[test]
fn rejects_path_traversal_in_layer() {
    let env = env();
    let sp = spec("detector", "1.0.0");
    let l = LayoutFiles::build(
        &serde_json::to_vec(&sp).unwrap(),
        MT_CONFIG,
        &[(
            evil_layer(b"../../../escape.sh", tar::EntryType::Regular, None),
            MT_LAYER_GZIP,
        )],
        "1.0.0",
    );
    let a = write(&env, &l, "traversal.tar");

    verify_err(env.mgr.install(&s(&a), false), "unsafe path");
    assert!(!env.tmp.path().join("escape.sh").exists());
    assert!(!env.root().join("escape.sh").exists());
    assert!(!env.app_dir("detector").exists());
}

#[test]
fn rejects_symlink_entries() {
    let env = env();
    let sp = spec("detector", "1.0.0");
    let l = LayoutFiles::build(
        &serde_json::to_vec(&sp).unwrap(),
        MT_CONFIG,
        &[(
            evil_layer(b"run.sh", tar::EntryType::Symlink, Some("/etc/shadow")),
            MT_LAYER_GZIP,
        )],
        "1.0.0",
    );
    let a = write(&env, &l, "symlink.tar");

    verify_err(env.mgr.install(&s(&a), false), "unsupported archive entry");
    assert!(!env.app_dir("detector").exists());
}

#[test]
fn rejects_invalid_app_spec() {
    let env = env();
    let mut sp = spec("detector", "1.0.0");
    sp.entrypoint.clear();
    let layer =
        edgeforge::pack::layer_from_dir(&app_source(&env, &spec("detector", "1.0.0"), SERVE))
            .unwrap();
    let l = LayoutFiles::build(
        &serde_json::to_vec(&sp).unwrap(),
        MT_CONFIG,
        &[(layer, MT_LAYER_GZIP)],
        "1.0.0",
    );
    let a = write(&env, &l, "badspec.tar");

    verify_err(
        env.mgr.install(&s(&a), false),
        "entrypoint must not be empty",
    );
}

#[test]
fn corrupted_update_leaves_running_version_untouched() {
    let env = env();
    running_v1(&env, "detector");
    let mut l = layout(&env, &spec("detector", "2.0.0"), SERVE);
    let key = blob_key(&l.layer_digests[0]);
    l.files.get_mut(&key).unwrap()[100] ^= 0xff;
    let a = write(&env, &l, "detector-2.0.0-corrupt.tar");

    assert!(matches!(
        env.mgr.update("detector", &s(&a)),
        Err(EdgeError::Verify(_))
    ));
    assert_still_v1(&env, "detector");
    assert_eq!(
        metric(
            &env.mgr,
            "edgeforge_updates_total{app=\"detector\",result=\"rejected\"}"
        ),
        1.0
    );
}

// ---------------------------------------------------------- network interruptions

#[test]
fn truncated_download_keeps_v1() {
    let env = env();
    running_v1(&env, "detector");
    let a = artifact(&env, &spec("detector", "2.0.0"), SERVE);
    let bytes = fs::read(&a).unwrap();
    let url = serve(bytes.clone(), Fault::TruncateAfter(bytes.len() / 3));

    match env.mgr.update("detector", &url) {
        Err(EdgeError::Fetch(m)) => assert!(m.contains("2 attempts"), "{m}"),
        other => panic!("expected fetch failure, got {other:?}"),
    }
    assert_still_v1(&env, "detector");
    assert!(fs::read_dir(env.root().join("staging"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn connection_refused_keeps_v1() {
    let env = env();
    running_v1(&env, "detector");

    assert!(matches!(
        env.mgr.update("detector", &dead_url()),
        Err(EdgeError::Fetch(_))
    ));
    assert_still_v1(&env, "detector");
    assert_eq!(
        metric(
            &env.mgr,
            "edgeforge_updates_total{app=\"detector\",result=\"fetch_failed\"}"
        ),
        1.0
    );
}

#[test]
fn stalled_server_times_out_and_keeps_v1() {
    let env = env();
    running_v1(&env, "detector");
    let url = serve(Vec::new(), Fault::Stall);

    let t = Instant::now();
    assert!(matches!(
        env.mgr.update("detector", &url),
        Err(EdgeError::Fetch(_))
    ));
    // 2 attempts x 800ms timeout + 50ms backoff, with headroom.
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "took {:?}",
        t.elapsed()
    );
    assert_still_v1(&env, "detector");
}

// ---------------------------------------------------------- bad releases

#[test]
fn crash_on_start_rolls_back_automatically() {
    let env = env();
    running_v1(&env, "detector");
    let v2 = artifact(&env, &spec("detector", "2.0.0"), CRASH);

    match env.mgr.update("detector", &s(&v2)) {
        Err(EdgeError::RolledBack(m)) => assert!(m.contains("exited during health gate"), "{m}"),
        other => panic!("expected rollback, got {other:?}"),
    }
    assert_still_v1(&env, "detector");
    let st = env.mgr.status("detector").unwrap();
    assert!(st.last_error.unwrap().contains("v2.0.0"));
    assert_eq!(
        metric(
            &env.mgr,
            "edgeforge_rollbacks_total{app=\"detector\",reason=\"health_gate\"}"
        ),
        1.0
    );
}

#[test]
fn failing_health_probe_rolls_back_after_grace() {
    let env = env();
    running_v1(&env, "detector");
    let mut v2 = spec("detector", "2.0.0");
    v2.health.probe = HealthProbe::Exec {
        command: vec!["sh".into(), "-c".into(), "exit 1".into()],
    };
    v2.health.grace_ms = 600;
    let a = artifact(&env, &v2, SERVE);

    let t = Instant::now();
    match env.mgr.update("detector", &s(&a)) {
        Err(EdgeError::RolledBack(m)) => assert!(m.contains("not healthy after 600ms"), "{m}"),
        other => panic!("expected rollback, got {other:?}"),
    }
    assert!(t.elapsed() >= Duration::from_millis(600));
    assert_still_v1(&env, "detector");
}

#[test]
fn crash_loop_is_detected_and_contained() {
    let env = env();
    let a = artifact(&env, &spec("detector", "1.0.0"), CRASH);
    env.mgr.install(&s(&a), true).unwrap();

    assert!(env.tick_until(Duration::from_secs(5), |m| {
        m.status("detector").unwrap().health == Health::CrashLoop
    }));
    let st = env.mgr.status("detector").unwrap();
    assert_eq!(st.restarts, 3, "max_restarts = 3");
    assert!(!st.running);
    // Parked: further ticks do not restart it.
    for _ in 0..10 {
        env.mgr.supervise_tick();
    }
    assert_eq!(env.mgr.status("detector").unwrap().restarts, 3);
    assert_eq!(
        metric(&env.mgr, "edgeforge_app_restarts_total{app=\"detector\"}"),
        3.0
    );
}

// ---------------------------------------------------------- power loss

#[test]
fn interrupted_update_is_rolled_back_on_boot() {
    let mut env = env();
    let a = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    env.mgr.install(&s(&a), false).unwrap();
    let dir = env.app_dir("detector");

    // Recreate the on-disk state of a device that lost power right after
    // `current` was switched to slot B but before the update was committed.
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap();
    state["pending"] = serde_json::json!({
        "from": {"slot": "a", "version": "1.0.0"},
        "to":   {"slot": "b", "version": "2.0.0"},
        "started_at": 1
    });
    fs::write(dir.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
    fs::write(dir.join("slots/b/run.sh"), "half-written").unwrap();
    edgeforge::fsutil::symlink_atomic(std::path::Path::new("slots/b"), &dir.join("current"))
        .unwrap();

    env.reopen();

    let st = env.mgr.status("detector").unwrap();
    assert_eq!(st.version, "1.0.0");
    assert_eq!(st.slot, Some(Slot::A));
    assert_eq!(
        fs::read_link(dir.join("current")).unwrap(),
        std::path::PathBuf::from("slots/a")
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert!(saved["pending"].is_null());
    assert_eq!(
        metric(
            &env.mgr,
            "edgeforge_rollbacks_total{app=\"detector\",reason=\"interrupted_update\"}"
        ),
        1.0
    );
    env.mgr.start("detector").unwrap();
    assert!(env.tick_until(Duration::from_secs(3), |m| m
        .status("detector")
        .unwrap()
        .health
        == Health::Healthy));
}

#[test]
fn stale_symlink_from_interrupted_swap_is_cleaned() {
    let mut env = env();
    let a = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    env.mgr.install(&s(&a), false).unwrap();
    let dir = env.app_dir("detector");
    std::os::unix::fs::symlink("slots/b", dir.join("current.tmp")).unwrap();

    env.reopen();

    assert!(fs::symlink_metadata(dir.join("current.tmp")).is_err());
    assert_eq!(env.mgr.status("detector").unwrap().version, "1.0.0");
}

#[test]
fn torn_state_write_never_corrupts_state_file() {
    // write_atomic goes through a temp file + rename, so a crash mid-write
    // leaves at most a stray temp file next to an intact state.json.
    let mut env = env();
    let a = artifact(&env, &spec("detector", "1.0.0"), SERVE);
    env.mgr.install(&s(&a), false).unwrap();
    let dir = env.app_dir("detector");
    let mut f = fs::File::create(dir.join(".state.json.tmp-999")).unwrap();
    f.write_all(b"{\"name\": \"detec").unwrap();
    drop(f);

    env.reopen();
    assert_eq!(env.mgr.status("detector").unwrap().version, "1.0.0");
}

#[test]
fn failed_update_never_offers_its_overwritten_slot_for_rollback() {
    // A/B has two slots: v3 is written over v1's slot. After v3 fails its
    // health gate, "previous" must not point at that slot any more —
    // otherwise a manual rollback would start v3's files labelled as v1.
    let env = env();
    running_v1(&env, "detector");
    let v2 = artifact(&env, &spec("detector", "2.0.0"), SERVE);
    env.mgr.update("detector", &s(&v2)).unwrap();
    let v3 = artifact(&env, &spec("detector", "3.0.0"), CRASH);

    assert!(matches!(
        env.mgr.update("detector", &s(&v3)),
        Err(EdgeError::RolledBack(_))
    ));
    let st = env.mgr.status("detector").unwrap();
    assert_eq!((st.version.as_str(), st.slot), ("2.0.0", Some(Slot::B)));
    assert_eq!(st.previous_version, None);
    assert!(matches!(
        env.mgr.rollback("detector"),
        Err(EdgeError::Invalid(_))
    ));
    assert!(st.running);
}
