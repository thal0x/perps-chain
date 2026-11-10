#!/usr/bin/env python3
"""AC3 measurement from a validator's commits.log + roots.log.

usage: ac3_measure.py NODE_HOME WARMUP_S WINDOW_S LOAD_JSON OUT FP DIRTY HEAD MODE [MIN_RATE]

* commits.log: `<height> <block_id> <timestamp_ms>` (block timestamp = leader's clock)
* roots.log:   `<height> <root> <order_actions>` (BlockOutcome.order_actions, D7)

t0 = timestamp of the first committed block with order_actions > 0. The window
is [t0 + warmup, t0 + warmup + window). actions_per_s = Σ order_actions of
blocks whose timestamp is in the window / window_s. min_10s_window = the
lowest Σ/10 over the window's consecutive (non-overlapping, aligned) 10 s
sub-windows and over every 10 s sliding window starting at each block (the
stricter of the two). median_block_ms = median of consecutive committed block
timestamp deltas inside the window.
"""
import json
import statistics
import sys
import datetime

home, warmup, window, load_json, out, fp, dirty, head, mode = sys.argv[1:10]
min_rate = int(sys.argv[10]) if len(sys.argv) > 10 else 100_000
warmup, window = int(warmup), int(window)

ts = {}
for line in open(f"{home}/commits.log"):
    p = line.split()
    if len(p) >= 3:
        ts[int(p[0])] = int(p[2])
oa = {}
for line in open(f"{home}/roots.log"):
    p = line.split()
    if len(p) >= 3:
        oa[int(p[0])] = int(p[2])
hs = sorted(h for h in ts if h in oa)
first = next((h for h in hs if oa[h] > 0), None)
res = {"validators": 4, "window_s": 0, "actions_per_s": 0, "min_10s_window": 0, "median_block_ms": None}
problems = []
if first is None:
    problems.append("no block with order actions")
else:
    t0 = ts[first] + warmup * 1000
    t1 = t0 + window * 1000
    inw = [h for h in hs if t0 <= ts[h] < t1]
    last_ts = ts[hs[-1]]
    if last_ts < t1:
        problems.append(f"chain stopped before the window ended ({(last_ts - t0) / 1000:.1f}s of {window}s)")
    total = sum(oa[h] for h in inw)
    aligned = []
    for k in range(window // 10):
        a, b = t0 + k * 10_000, t0 + (k + 1) * 10_000
        aligned.append(sum(oa[h] for h in inw if a <= ts[h] < b) / 10)
    sliding = []
    for i, h in enumerate(inw):
        if ts[h] + 10_000 > t1:
            break
        sliding.append(sum(oa[x] for x in inw[i:] if ts[x] < ts[h] + 10_000) / 10)
    deltas = [ts[b] - ts[a] for a, b in zip(inw, inw[1:])]
    res.update({
        "window_s": window if not problems else (min(last_ts, t1) - t0) // 1000,
        "actions_per_s": round(total / window),
        "min_10s_window": round(min(aligned + sliding)) if aligned else 0,
        "min_10s_aligned": round(min(aligned)) if aligned else 0,
        "min_10s_sliding": round(min(sliding)) if sliding else None,
        "aligned_10s_windows": [round(x) for x in aligned],
        "median_block_ms": statistics.median(deltas) if deltas else None,
        "p90_block_ms": sorted(deltas)[int(len(deltas) * 0.9)] if deltas else None,
        "max_block_ms": max(deltas) if deltas else None,
        "blocks_in_window": len(inw),
        "order_actions_in_window": total,
        "mean_actions_per_block": round(total / max(1, len(inw))),
        "first_load_height": first,
    })
try:
    load = json.load(open(load_json))
except Exception as e:  # noqa
    load = {"error": str(e)}
    problems.append("load report missing")
win_ok = res["window_s"] >= (120 if mode == "full" else 20)
ok = (not problems and win_ok and res["actions_per_s"] >= min_rate and res["min_10s_window"] >= min_rate * 9 // 10
      and res["median_block_ms"] is not None and res["median_block_ms"] < 1000)
res.update({
    "ac": "ac3", "mode": mode, "pass": ok, "threshold_actions_per_s": min_rate,
    "problems": problems, "load": load,
    "source_fingerprint": fp, "git_head": head, "git_dirty": dirty == "true",
    "time_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "summary": (f"{res['actions_per_s']} order actions/s over {res['window_s']}s (min 10s window {res['min_10s_window']}), "
                f"median block {res['median_block_ms']} ms, 4 validators" + (f"; {'; '.join(problems)}" if problems else "")),
})
json.dump(res, open(out, "w"), indent=1)
print(f"AC3 {'PASS' if ok else 'FAIL'}: {res['summary']}  -> {out}")
sys.exit(0 if ok else 1)
