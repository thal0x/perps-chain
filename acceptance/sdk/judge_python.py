#!/usr/bin/env python3
"""Judge one AC5 Python example run (TEST_PLAN AC5, D23).

usage: judge_python.py --script NAME --dir RUN_DIR --allowlist expected_errors.json
                       [--ws --min-ws-s 30]
RUN_DIR holds NAME.stdout, NAME.stderr, NAME.trace (JSON lines from
py_trace/sitecustomize.py), NAME.meta.json ({"rc", "elapsed_s", "alive_at_stop"}).
Prints one JSON object (the per-script verdict) on stdout.

Rules
* normal example: rc == 0, no Python traceback on stderr.
* long-running WS example (--ws): still alive when stopped after >= min-ws-s,
  no traceback, and every subscription the SDK sent received >= 1 message that
  the SDK routed to it (identifier from the SDK's own ws_msg_to_identifier).
* every /exchange response that is an error -- top-level {"status": "err"} or a
  per-order {"error": ...} in response.data.statuses -- must match an allowlist
  entry (script, call, exact error_text); otherwise the script fails.
* every allowlist entry for this script must actually occur (no hidden drift).
* backstop: if stdout shows 'status': 'err' / "status": "err" but the trace
  recorded no top-level error, the script fails (untraced error).
"""
import argparse
import json
import os
import re
import sys


def load_lines(p):
    out = []
    if os.path.exists(p):
        for line in open(p, errors="replace"):
            line = line.strip()
            if line:
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass
    return out


def errors_of(resp):
    """List of error texts in an /exchange response."""
    errs = []
    if not isinstance(resp, dict):
        return [f"non-dict response: {resp!r}"]
    if resp.get("status") == "err":
        errs.append(("top", str(resp.get("response"))))
        return errs
    r = resp.get("response")
    if isinstance(r, dict):
        d = r.get("data")
        if isinstance(d, dict):
            for s in d.get("statuses", []) or []:
                if isinstance(s, dict) and "error" in s:
                    errs.append(("status", str(s["error"])))
            st = d.get("status")
            if isinstance(st, dict) and "error" in st:
                errs.append(("status", str(st["error"])))
    return errs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--script", required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--allowlist", required=True)
    ap.add_argument("--ws", action="store_true")
    ap.add_argument("--min-ws-s", type=float, default=30.0)
    a = ap.parse_args()
    base = os.path.join(a.dir, a.script)
    stdout = open(base + ".stdout", errors="replace").read() if os.path.exists(base + ".stdout") else ""
    stderr = open(base + ".stderr", errors="replace").read() if os.path.exists(base + ".stderr") else ""
    meta = json.load(open(base + ".meta.json"))
    trace = load_lines(base + ".trace")
    allow = [e for e in json.load(open(a.allowlist)) if e["script"] == a.script]

    reasons = []
    if a.ws:
        if not meta.get("alive_at_stop"):
            reasons.append(f"exited before the {a.min_ws_s:.0f}s window (rc={meta.get('rc')})")
        if meta.get("elapsed_s", 0) < a.min_ws_s:
            reasons.append(f"ran only {meta.get('elapsed_s')}s < {a.min_ws_s}s")
    elif meta.get("rc") != 0:
        reasons.append(f"exit code {meta.get('rc')}")
    if "Traceback (most recent call last)" in stderr or "Traceback (most recent call last)" in stdout:
        reasons.append("python traceback in output")

    # /exchange errors
    calls = [t for t in trace if "path" in t]
    exch = [t for t in calls if t["path"] == "/exchange"]
    seen_errors = []
    unexpected = []
    matched = set()
    top_errs = 0
    for t in exch:
        if "exception" in t:
            unexpected.append({"call": t.get("call"), "request": t.get("request"), "exception": t["exception"]})
            continue
        for kind, text in errors_of(t.get("response")):
            if kind == "top":
                top_errs += 1
            ent = {"call": t.get("call"), "error_text": text, "request": t.get("request"), "response": t.get("response")}
            seen_errors.append(ent)
            hit = [i for i, e in enumerate(allow) if e["call"] == t.get("call") and e["error_text"] == text]
            if hit:
                matched.update(hit)
                ent["allowlisted"] = True
            else:
                unexpected.append(ent)
    for t in calls:
        if t["path"] != "/exchange" and "exception" in t:
            unexpected.append({"call": t.get("call"), "request": t.get("request"), "exception": t["exception"]})
    if unexpected:
        reasons.append(f"{len(unexpected)} unexpected error(s)")
    for i, e in enumerate(allow):
        if i not in matched:
            reasons.append(f"allowlisted error did not occur: {e['call']}: {e['error_text']}")
    if re.search(r"""['"]status['"]: ['"]err['"]""", stdout) and top_errs == 0:
        reasons.append("stdout shows status err but no traced error response")

    ws = None
    if a.ws:
        subs = sorted({t["ws_sub"] for t in trace if "ws_sub" in t})
        got = {}
        undelivered = {}
        for t in trace:
            if "ws_msg" in t or "channel" in t:
                if t.get("ws_msg") and t.get("dispatched"):
                    got[t["ws_msg"]] = got.get(t["ws_msg"], 0) + 1
                elif t.get("channel") not in (None, "subscriptionResponse", "pong"):
                    k = f"{t.get('channel')}:{t.get('ws_msg')}"
                    undelivered[k] = undelivered.get(k, 0) + 1
        missing = [s for s in subs if got.get(s, 0) == 0]
        if not subs:
            reasons.append("no websocket subscriptions were sent")
        if missing:
            reasons.append(f"no message on subscribed channel(s): {', '.join(missing)}")
        ws = {"subscriptions": subs, "messages": {s: got.get(s, 0) for s in subs},
              "missing": missing, "undelivered_messages": undelivered}

    v = {
        "script": a.script,
        "pass": not reasons,
        "rc": meta.get("rc"),
        "elapsed_s": meta.get("elapsed_s"),
        "exchange_calls": len(exch),
        "info_calls": len(calls) - len(exch),
        "errors": seen_errors,
        "unexpected_errors": unexpected,
        "reasons": reasons,
    }
    if ws is not None:
        v["ws"] = ws
        v["alive_at_stop"] = meta.get("alive_at_stop")
    if "fixture" in meta:
        v["fixture"] = meta["fixture"]
        if meta["fixture"].get("rc") != 0:
            v["pass"] = False
            v["reasons"].append(f"fixture {meta['fixture'].get('name')} failed (rc={meta['fixture'].get('rc')})")
    json.dump(v, sys.stdout)
    print()


if __name__ == "__main__":
    main()
