#!/usr/bin/env python3
"""Count the property-test cases executed by `cargo test -p clearinghouse -p state`.

usage: proptest_cases.py CARGO_TEST_LOG OUT_JSON crates/clearinghouse crates/state ...

Proptest does not print case counts, so they are derived from the sources:
every `proptest! { ... }` block's `#![proptest_config(...)]` gives `cases`
(explicit number, `with_cases(N)`, or the default = $PROPTEST_CASES or 256),
and every `#[test] fn` inside it runs exactly that many passing cases when it
passes (rejected inputs are not counted by proptest). Each such test must
appear as `test <name> ... ok` in the cargo log, otherwise it counts as failed.
"""
import json
import os
import re
import sys

log_path, out_path, *dirs = sys.argv[1:]
log = open(log_path, errors="replace").read()
default_cases = int(os.environ.get("PROPTEST_CASES", "256"))


def blocks(src):
    """Yield the text of each proptest! { ... } block (brace matched)."""
    for m in re.finditer(r"proptest!\s*\{", src):
        i, depth = m.end(), 1
        while i < len(src) and depth:
            depth += {"{": 1, "}": -1}.get(src[i], 0)
            i += 1
        yield src[m.end():i - 1]


breakdown = []
for d in dirs:
    for root, _, files in os.walk(d):
        for f in sorted(files):
            if not f.endswith(".rs"):
                continue
            p = os.path.join(root, f)
            src = open(p).read()
            for b in blocks(src):
                cfg = re.search(r"#!\[proptest_config\((.*?)\)\]\s*$", b, re.M | re.S)
                cases = default_cases
                if cfg:
                    c = re.search(r"cases\s*:\s*([0-9_]+)", cfg.group(1)) or re.search(r"with_cases\(\s*([0-9_]+)", cfg.group(1))
                    if c:
                        cases = int(c.group(1).replace("_", ""))
                for t in re.finditer(r"#\[test\]\s*(?:#\[[^\]]*\]\s*)*fn\s+(\w+)", b):
                    name = t.group(1)
                    ok = re.search(rf"^test (\S+::)?{name} \.\.\. ok$", log, re.M) is not None
                    breakdown.append({"file": os.path.relpath(p), "test": name, "cases": cases, "passed": ok})

res = {
    "tests": len(breakdown),
    "failed": sum(1 for b in breakdown if not b["passed"]),
    "cases": sum(b["cases"] for b in breakdown if b["passed"]),
    "cases_per_test": sorted({b["cases"] for b in breakdown}),
    "breakdown": breakdown,
    "cargo_test_failed": len(re.findall(r"^test .* \.\.\. FAILED$", log, re.M)),
}
if res["cargo_test_failed"]:
    res["failed"] += res["cargo_test_failed"]
json.dump(res, open(out_path, "w"), indent=1)
print(f"proptests: {res['tests']} tests, {res['cases']} cases, {res['failed']} failed")
