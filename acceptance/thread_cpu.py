#!/usr/bin/env python3
"""Per-thread CPU of running processes (a poor man's profiler; no perf here).

usage: thread_cpu.py SECONDS OUT_JSON PID [PID...]
Samples /proc/<pid>/task/*/stat (utime+stime) at start and end, aggregates
by thread name (digits stripped, e.g. "peer-txs-0" -> "peer-txs-"), prints a
table of CPU-seconds per second (1.0 = one core) per process and in total.
"""
import json
import os
import re
import sys
import time

HZ = os.sysconf("SC_CLK_TCK")


def sample(pid):
    out = {}
    base = f"/proc/{pid}/task"
    for t in os.listdir(base):
        try:
            comm = open(f"{base}/{t}/comm").read().strip()
            st = open(f"{base}/{t}/stat").read()
            f = st[st.rindex(")") + 2:].split()
            out[t] = (comm, int(f[11]) + int(f[12]))
        except (FileNotFoundError, ProcessLookupError, ValueError):
            pass
    return out


secs = float(sys.argv[1])
out_path = sys.argv[2]
pids = sys.argv[3:]
a = {p: sample(p) for p in pids}
t0 = time.time()
time.sleep(secs)
b = {p: sample(p) for p in pids}
dt = time.time() - t0
res = {"seconds": dt, "per_process": {}, "total_by_thread": {}}
for p in pids:
    agg = {}
    for t, (comm, ticks) in b[p].items():
        prev = a[p].get(t, (comm, 0))[1]
        name = re.sub(r"\d+$", "", comm)
        agg[name] = agg.get(name, 0) + (ticks - prev) / HZ / dt
    res["per_process"][p] = dict(sorted(agg.items(), key=lambda x: -x[1]))
    for k, v in agg.items():
        res["total_by_thread"][k] = res["total_by_thread"].get(k, 0) + v
res["total_by_thread"] = {k: round(v, 3) for k, v in sorted(res["total_by_thread"].items(), key=lambda x: -x[1])}
res["total_cores"] = round(sum(res["total_by_thread"].values()), 3)
json.dump(res, open(out_path, "w"), indent=1)
for k, v in res["total_by_thread"].items():
    if v >= 0.005:
        print(f"{k:24s} {v:6.3f} cores")
print(f"{'TOTAL':24s} {res['total_cores']:6.3f} cores over {dt:.1f}s")
