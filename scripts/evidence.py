#!/usr/bin/env python3
"""Evaluate acceptance evidence for one AC.

usage: evidence.py acN <smoke: ok|fail|SKIP>
prints "RESULT|smoke|evidence-summary" where RESULT in PASS/STALE/FAIL.

Evidence lives in acceptance/results/acN.json and must contain:
  pass (bool), source_fingerprint (str), plus the per-AC fields checked below.
"""
import json, os, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def fingerprint():
    """Tree hashes of all state-relevant sources at HEAD (+dirty marker)."""
    parts = []
    for p in ["crates", "bridge", "Cargo.lock", "acceptance/sdk"]:
        r = subprocess.run(["git", "rev-parse", f"HEAD:{p}"], cwd=ROOT, capture_output=True, text=True)
        parts.append(r.stdout.strip() or "-")
    return ":".join(x[:12] for x in parts)


THRESHOLDS = {
    "ac1": lambda e: e.get("duration_s", 0) >= 43200 and e.get("validators") == 7
    and e.get("safety_violations", 1) == 0 and e.get("max_resume_ms", 10**9) <= 10000
    and e.get("heals", 0) > 0,
    "ac2": lambda e: e.get("actions", 0) >= 50_000_000 and e.get("sim_days", 0) >= 7
    and e.get("runs", 0) >= 2 and e.get("mismatches", 1) == 0 and e.get("validators", 0) >= 4,
    "ac3": lambda e: e.get("validators") == 4 and e.get("window_s", 0) >= 120
    and e.get("actions_per_s", 0) >= 100_000 and e.get("median_block_ms", 10**9) < 1000
    and e.get("min_10s_window", 0) >= 90_000,
    "ac4": lambda e: e.get("violations", 1) == 0 and e.get("replay_blocks_checked", 0) > 0
    and e.get("proptest_cases", 0) > 0,
    "ac5": lambda e: e.get("python_failed", 1) == 0 and e.get("ts_failed", 1) == 0
    and e.get("python_passed", 0) > 0 and e.get("ts_passed", 0) > 0,
    "ac6": lambda e: e.get("e2e_pass") is True and e.get("forge_failed", 1) == 0,
}


def main():
    ac, smoke = sys.argv[1], sys.argv[2]
    path = os.path.join(ROOT, "acceptance", "results", f"{ac}.json")
    if not os.path.exists(path):
        print(f"FAIL|{smoke}|missing")
        return
    try:
        e = json.load(open(path))
    except Exception as ex:  # noqa
        print(f"FAIL|{smoke}|unreadable")
        return
    meets = bool(e.get("pass")) and THRESHOLDS[ac](e)
    fresh = e.get("source_fingerprint") == fingerprint()
    summary = e.get("summary", "")
    if not meets:
        print(f"FAIL|{smoke}|thresholds not met {summary}")
    elif smoke == "fail":
        print(f"FAIL|{smoke}|evidence ok but smoke fails")
    elif not fresh:
        print(f"STALE|{smoke}|ran on {e.get('source_fingerprint')} {summary}")
    else:
        print(f"PASS|{smoke}|{summary}")


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--fingerprint":
        print(fingerprint())
    else:
        main()
