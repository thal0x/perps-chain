//! End-to-end runs of `hlchaos` against the fake node (src/bin/fake_hlnode.rs).

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

fn workdir(tag: &str) -> PathBuf {
    chaos::testutil::tempdir(tag)
}

/// Run hlchaos; returns (exit code, report json, stdout).
fn hlchaos(tag: &str, extra: &[&str], env: &[(&str, &str)]) -> (i32, Value, String) {
    let wd = workdir(tag);
    let report = wd.join("report.json");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hlchaos"));
    cmd.args(["run", "--hlnode", env!("CARGO_BIN_EXE_fake-hlnode")])
        .arg("--workdir")
        .arg(&wd)
        .arg("--report")
        .arg(&report)
        .args(["--progress-every", "2s", "--startup-timeout", "20s"])
        .args(extra);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run hlchaos");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    let r: Value = serde_json::from_slice(&std::fs::read(&report).expect("report written")).expect("report json");
    // node dirs are removed by default; the report stays
    let runs: Vec<_> = std::fs::read_dir(&wd).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("run-")).collect();
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].path().join("net").exists(), "net dir should be cleaned up");
    let _ = std::fs::remove_dir_all(&wd);
    (out.status.code().unwrap_or(-1), r, stdout)
}

#[test]
fn mixed_episodes_pass_on_honest_fake_nodes() {
    let (code, r, out) = hlchaos(
        "mixed",
        &["--validators", "7", "--max-faulty", "2", "--seed", "7", "--duration", "40s", "--episode-min", "3s", "--episode-max", "5s",
          "--observe", "2s", "--stall-window", "8s", "--load-rate", "20",
          "--kinds", "kill,rolling_kill,isolate,minority_partition,lossy,jitter,combo,split"],
        &[],
    );
    assert_eq!(r["pass"], true, "{}", r["summary"]);
    assert_eq!(code, 0);
    assert_eq!(r["validators"], 7);
    assert_eq!(r["safety_violations"], 0);
    assert!(r["episodes"].as_u64().unwrap() >= 4, "{}", r["episodes"]);
    assert_eq!(r["heals"], r["episodes"]);
    assert!(r["max_resume_ms"].as_u64().unwrap() <= 10_000);
    assert!(r["duration_s"].as_u64().unwrap() >= 40);
    assert!(r["checker"]["cross_checked_commits"].as_u64().unwrap() > 50);
    assert_eq!(r["complete"], true);
    assert!(r["load"]["accepted"].as_u64().unwrap() > 10, "{}", r["load"]);
    assert!(out.contains("== episode 1"));
    for k in ["duration_s", "validators", "episodes", "heals", "max_resume_ms", "p50_resume_ms", "p99_resume_ms",
              "safety_violations", "kills", "partitions", "pass", "seed", "summary"] {
        assert!(r.get(k).is_some(), "missing {k}");
    }
}

#[test]
fn split_resume_is_measured() {
    // halting split; the fake waits 1.5 s after the heal before committing
    let (code, r, _) = hlchaos(
        "split",
        &["--validators", "4", "--max-faulty", "1", "--seed", "3", "--duration", "20s", "--episode-min", "3s", "--episode-max", "3s",
          "--observe", "3s", "--kinds", "split", "--load-rate", "0"],
        &[("FAKE_RESUME_DELAY_MS", "1500")],
    );
    assert_eq!(code, 0, "{}", r["summary"]);
    assert!(r["splits"].as_u64().unwrap() >= 2);
    for h in r["heal_records"].as_array().unwrap() {
        let ms = h["resume_ms"].as_u64().unwrap();
        assert!((1500..4000).contains(&ms), "resume {ms}");
        assert_eq!(h["halting"], true);
    }
    assert!(r["p50_resume_ms"].as_u64().unwrap() >= 1500);
    assert_eq!(r["violations_by_kind"].as_object().map(|o| o.len()).unwrap_or(0), 0);
}

#[test]
fn slow_resume_fails_liveness() {
    let (code, r, out) = hlchaos(
        "slow",
        &["--validators", "4", "--max-faulty", "1", "--seed", "4", "--duration", "14s", "--episode-min", "3s", "--episode-max", "3s",
          "--observe", "2s", "--kinds", "split", "--resume-limit-ms", "1000", "--load-rate", "0"],
        &[("FAKE_RESUME_DELAY_MS", "2500")],
    );
    assert_eq!(code, 1);
    assert_eq!(r["pass"], false);
    assert!(r["max_resume_ms"].as_u64().unwrap() >= 2500);
    assert!(out.contains("LIVENESS FAILURE"));
}

#[test]
fn divergence_and_conflicts_fail_safety() {
    let (code, r, out) = hlchaos(
        "diverge",
        &["--validators", "4", "--max-faulty", "1", "--seed", "5", "--duration", "12s", "--episode-min", "3s", "--episode-max", "3s",
          "--observe", "2s", "--kinds", "jitter", "--load-rate", "0"],
        &[("FAKE_DIVERGE_NODE", "2"), ("FAKE_DIVERGE_HEIGHT", "40"), ("FAKE_CONFLICT_NODE", "1"), ("FAKE_CONFLICT_HEIGHT", "45")],
    );
    assert_eq!(code, 1);
    assert_eq!(r["pass"], false);
    let by = &r["violations_by_kind"];
    assert_eq!(by["state_divergence"], 1, "{by}");
    // one per disagreeing (node, height) pair: 1 if the bad node reported
    // last, up to n-1 if it reported first
    assert!((1..=3).contains(&by["root_mismatch"].as_u64().unwrap()), "{by}");
    assert!((1..=3).contains(&by["conflicting_commit"].as_u64().unwrap()), "{by}");
    assert!(out.contains("SAFETY VIOLATION"));
    assert!(r["summary"].as_str().unwrap().starts_with("FAIL"));
}

#[test]
fn startup_failure_is_reported() {
    let wd = workdir("nonode");
    let out = Command::new(env!("CARGO_BIN_EXE_hlchaos"))
        .args(["run", "--hlnode", "/nonexistent/hlnode", "--duration", "5s"])
        .arg("--workdir")
        .arg(&wd)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("testnet-gen"));
    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn plan_subcommand_prints_schedule() {
    let out = Command::new(env!("CARGO_BIN_EXE_hlchaos"))
        .args(["plan", "--validators", "7", "--seed", "9", "--episodes", "5"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.lines().count(), 5);
    for l in s.lines() {
        let v: Value = serde_json::from_str(l).unwrap();
        assert!(v["duration_ms"].as_u64().unwrap() >= 20_000);
    }
}
