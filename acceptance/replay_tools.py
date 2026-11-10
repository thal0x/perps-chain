#!/usr/bin/env python3
"""Helpers for scripts/acceptance.sh AC2 / AC4 (block-log replay).

  replay_tools.py homes AC2_DIR RUN_DIR      -> node homes (validator.key, p2p) from keys.json/genesis.json
  replay_tools.py ac2 AC2_DIR OUT FP DIRTY HEAD MODE DAYS
  replay_tools.py ac4 OUT FP DIRTY HEAD MODE PROPTEST_JSON INVARIANTS_JSON
"""
import datetime
import gzip
import json
import os
import sys


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def homes(d, r):
    k = json.load(open(os.path.join(d, "keys.json")))
    g = json.load(open(os.path.join(d, "genesis.json")))
    for i, v in enumerate(k["validators"]):
        h = os.path.join(r, f"node{i}")
        os.makedirs(h, exist_ok=True)
        open(os.path.join(h, "validator.key"), "w").write(v["secret"])
        open(os.path.join(h, "p2p"), "w").write(g["validators"]["validators"][i]["p2p_addr"])


def norm(x):
    x = x.lower()
    return x[2:] if x.startswith("0x") else x


def roots(p):
    m = {}
    with gzip.open(p, "rt") as f:
        for line in f:
            parts = line.split()
            if len(parts) >= 2:
                m[int(parts[0])] = norm(parts[1])
    return m


def phases(d):
    p = os.path.join(d, "phases.tsv")
    if not os.path.exists(p):
        return []
    out = []
    for line in open(p):
        f = line.rstrip("\n").split("\t")
        if len(f) >= 2:
            out.append({"phase": f[0], "seconds": int(f[1]), "info": f[2] if len(f) > 2 else ""})
    return out


def ac2(d, out, fp, dirty, head, mode, days):
    gen = json.load(open(os.path.join(d, "gen_summary.json")))
    off = json.load(open(os.path.join(d, "offline_summary.json")))
    blocks = gen["footer"]["blocks"]
    runs = "AB"
    sources = {f"{r}{i}": roots(os.path.join(d, f"roots_{r}_node{i}.txt.gz")) for r in runs for i in range(4)}
    sources["offline"] = roots(os.path.join(d, "offline_roots.txt.gz"))
    sources["gen"] = roots(os.path.join(d, "gen_roots.txt.gz"))
    want = set(range(1, blocks + 1))
    mismatches, first = 0, []
    for h in range(1, blocks + 1):
        vals = {k: v.get(h) for k, v in sources.items()}
        if None in vals.values() or len(set(vals.values())) != 1:
            mismatches += 1
            if len(first) < 10:
                first.append({"height": h, "roots": vals})
    extra = {k: sorted(set(v) - want)[:5] for k, v in sources.items() if set(v) - want}
    mismatches += sum(len(set(v) - want) for v in sources.values())
    reps = {f"{r}{i}": json.load(open(os.path.join(d, f"replay_{r}_node{i}.json"))) for r in runs for i in range(4)}
    divs = {f"{r}{i}": int(open(os.path.join(d, f"divergence_{r}_node{i}.txt")).read().strip() or 0) for r in runs for i in range(4)}
    off_digest = norm(off["roots_digest"])
    digests = {k: norm(v["roots_digest"]) for k, v in reps.items()}
    final_ok = all(v["last_height"] == blocks for v in reps.values()) and all(s and max(s) == blocks for s in sources.values())
    digest_ok = all(x == off_digest for x in digests.values())
    min_actions = 50_000_000 if mode == "full" else 200_000
    ok = (mismatches == 0 and final_ok and digest_ok and sum(divs.values()) == 0 and gen["footer"]["actions"] >= min_actions)
    summary = (f"{gen['footer']['actions']} actions / {blocks} blocks / {days} sim-days; 2 runs x 4 validators "
               f"+ offline sim replay + gen roots compared per height; {mismatches} mismatches; roots_digest {off_digest[:16]}")
    ev = {
        "ac": "ac2", "mode": mode, "pass": ok,
        "actions": gen["footer"]["actions"], "order_actions": gen["footer"]["order_actions"],
        "sim_days": float(days), "blocks": blocks, "validators": 4, "runs": 2,
        "mismatches": mismatches, "roots_digest": off_digest,
        "final_height_ok": final_ok, "digests_equal": digest_ok, "divergence_log_lines": divs,
        "first_mismatches": first, "out_of_range_heights": extra,
        "node_replay": {k: {x: v[x] for x in ("last_height", "invariant_checks", "violations", "wall_ms", "order_actions")}
                        for k, v in reps.items()},
        "offline": {x: off[x] for x in ("blocks", "invariant_checks", "order_actions", "wall_ms")} | {"violations": len(off["violations"])},
        "gen": {"wall_ms": gen["wall_ms"], "log_bytes": os.path.getsize(os.path.join(d, "log.zst")) if os.path.exists(os.path.join(d, "log.zst")) else None},
        "phases": phases(d),
        "source_fingerprint": fp, "git_head": head, "git_dirty": dirty == "true", "time_utc": now(),
        "summary": summary,
    }
    json.dump(ev, open(out, "w"), indent=1)
    # invariant data for AC4 (reused when fingerprint + mode match)
    inv = {
        "mode": mode, "source_fingerprint": fp, "blocks": blocks, "actions": gen["footer"]["actions"],
        "offline_checks": off["invariant_checks"], "offline_violations": off["violations"][:100],
        "node_checks": {k: v["invariant_checks"] for k, v in reps.items()},
        "node_violations": {k: v["violations"] for k, v in reps.items()},
        "node_first_violations": {k: v["first_violations"][:10] for k, v in reps.items() if v["violations"]},
        "complete": final_ok,
    }
    json.dump(inv, open(os.path.join(d, "invariants.json"), "w"), indent=1)
    print(f"AC2 {'PASS' if ok else 'FAIL'}: {summary}  -> {out}")
    return 0 if ok else 1


def ac4(out, fp, dirty, head, mode, prop_json, inv_json):
    prop = json.load(open(prop_json))
    inv = json.load(open(inv_json))
    node_checks = inv["node_checks"]
    replay_checked = min(list(node_checks.values()) + [inv["offline_checks"]])
    viol = len(inv["offline_violations"]) + sum(inv["node_violations"].values()) + prop["failed"]
    ok = (viol == 0 and prop["failed"] == 0 and prop["cases"] > 0 and inv.get("complete", False)
          and replay_checked == inv["blocks"] and inv["source_fingerprint"] == fp)
    summary = (f"proptests {prop['tests']} tests / {prop['cases']} cases ({prop['failed']} failed); "
               f"invariants every block of {inv['blocks']} blocks ({inv['actions']} actions): offline sim replay + "
               f"{len(node_checks)} node replays; {viol} violations")
    ev = {
        "ac": "ac4", "mode": mode, "pass": ok,
        "proptest_cases": prop["cases"], "proptest_tests": prop["tests"], "proptest_failed": prop["failed"],
        "proptest_cases_per_test": prop["cases_per_test"], "proptest_breakdown": prop["breakdown"],
        "replay_blocks_checked": replay_checked, "replay_blocks": inv["blocks"], "replay_actions": inv["actions"],
        "replay_node_checks": node_checks, "replay_offline_checks": inv["offline_checks"],
        "violations": viol, "first_violations": inv["offline_violations"][:10], "node_first_violations": inv["node_first_violations"],
        "source_fingerprint": fp, "git_head": head, "git_dirty": dirty == "true", "time_utc": now(), "summary": summary,
    }
    json.dump(ev, open(out, "w"), indent=1)
    print(f"AC4 {'PASS' if ok else 'FAIL'}: {summary}  -> {out}")
    return 0 if ok else 1


if __name__ == "__main__":
    cmd, args = sys.argv[1], sys.argv[2:]
    if cmd == "homes":
        homes(*args)
    elif cmd == "ac2":
        sys.exit(ac2(*args))
    elif cmd == "ac4":
        sys.exit(ac4(*args))
    else:
        sys.exit(f"unknown command {cmd}")
