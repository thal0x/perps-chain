#!/usr/bin/env python3
"""Check that every `// --- BEGIN verbatim <source> [<anchor>]` ... `// --- END verbatim`
region in acceptance/sdk/ts/*.ts is the upstream snippet with only the documented
substitutions (transport URL/testnet flag, placeholder key/address/oid, Deno npm:
specifier).

usage: check_ts_verbatim.py <ts_dir> <ts_sdk_clone>
Sources: README.md "<heading>", docs/guides/market-orders.md, and the @example of
`client.ts <Class>.<method>` (src/api/exchange/client.ts, src/api/subscription/client.ts).
Prints a JSON summary; exit 1 on any mismatch.
"""
import glob
import json
import os
import re
import sys

ts_dir, clone = sys.argv[1], sys.argv[2]

LOCAL_HTTP = '{ apiUrl: "http://127.0.0.1:3001", isTestnet: true }'
LOCAL_WS = '{ url: "ws://127.0.0.1:3001/ws", isTestnet: true }'
REVERSE = [
    (f"HttpTransport({LOCAL_HTTP})", "HttpTransport()"),
    (f"WebSocketTransport({LOCAL_WS})", "WebSocketTransport()"),
    ("privateKeyToAccount(EXAMPLE_KEY)", 'privateKeyToAccount("0x...")'),
    ("user: EXAMPLE_ADDRESS", 'user: "0x..."'),
    ("destination: EXAMPLE_ADDRESS", 'destination: "0x..."'),
    ('destination: "0x0000000000000000000000000000000000000000"', 'destination: "0x..."'),
    ("oid: OID", "oid: 123"),
    ("o: OID", "o: 123"),
]
DENO = ('from "viem/accounts"', 'from "npm:viem/accounts"')


def code_blocks(md):
    return re.findall(r"```ts\n(.*?)```", md, re.S)


def readme_section(heading):
    md = open(os.path.join(clone, "README.md")).read()
    i = md.index(f"### {heading}")
    return code_blocks(md[i:])[0]


def jsdoc_example(path, cls, method):
    src = open(os.path.join(clone, path)).read()
    ci = src.index(f"class {cls}")
    m = re.search(rf"\n  {method}\(", src[ci:])
    head = src[ci:ci + m.start()]
    j = head.rindex("@example")
    block = head[j:]
    lines = [re.sub(r"^\s*\* ?", "", l) for l in block.split("\n")]
    txt = "\n".join(lines)
    return code_blocks(txt)[0]


def source_text(label):
    if label.startswith("README.md"):
        return readme_section(re.search(r'"(.*)"', label).group(1)), False
    if label.startswith("docs/"):
        return code_blocks(open(os.path.join(clone, label)).read())[0], False
    m = re.match(r"client\.ts (\w+)\.(\w+)", label)
    if m:
        cls, meth = m.groups()
        path = "src/api/exchange/client.ts" if cls == "ExchangeClient" else "src/api/subscription/client.ts"
        return jsdoc_example(path, cls, meth), True
    raise ValueError(f"unknown source label {label!r}")


def norm(s):
    return "\n".join(l.rstrip() for l in s.strip("\n").split("\n"))


ok = True
report = []
for f in sorted(glob.glob(os.path.join(ts_dir, "*.ts"))):
    text = open(f).read()
    for m in re.finditer(r"// --- BEGIN verbatim (.*?)\n(.*?)// --- END verbatim", text, re.S):
        label, body = m.group(1).strip(), m.group(2)
        src, deno = source_text(label)
        rev, nsub = body, 0
        for a, b in REVERSE:
            nsub += rev.count(a)
            rev = rev.replace(a, b)
        if deno:
            nsub += rev.count(DENO[0])
            rev = rev.replace(DENO[0], DENO[1])
        good = norm(rev) in norm(src)
        ok &= good
        report.append({"file": os.path.basename(f), "source": label, "verbatim": good, "substitutions": nsub})
        if not good:
            sys.stderr.write(f"VERBATIM CHECK FAILED: {os.path.basename(f)} [{label}]\n--- expected (upstream) ---\n{src}\n--- got (reversed) ---\n{rev}\n")
files_with_regions = {r["file"] for r in report}
print(json.dumps({"ok": ok, "regions": report}))
sys.exit(0 if ok and report else 1)
