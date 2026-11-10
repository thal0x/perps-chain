# STATUS  (commit , 2026-11-10 13:50 UTC)

| kind | suite | passed | failed | result |
|---|---|---:|---:|---|
| build | cargo build --workspace | 1 | 0 | PASS |
| crate | interfaces | 3 | 0 | PASS |
| crate | consensus | 0 | 0 | FAIL |
| crate | net | 0 | 0 | FAIL |
| crate | mempool | 0 | 0 | FAIL |
| crate | book | 0 | 0 | FAIL |
| crate | clearinghouse | 0 | 0 | FAIL |
| crate | oracle | 0 | 0 | FAIL |
| crate | state | 0 | 0 | FAIL |
| crate | hlwire | 0 | 0 | FAIL |
| crate | api | 0 | 0 | FAIL |
| crate | relayer | 0 | 0 | FAIL |
| crate | sim | 0 | 0 | FAIL |
| crate | node | 0 | 0 | FAIL |
| crate | chaos | 0 | 0 | FAIL |
| suite | forge (bridge) | 0 | 1 | FAIL |
| suite | no-float | 1 | 0 | PASS |

| AC | criterion | smoke | evidence | result |
|---|---|---|---|---|
| AC1 | consensus safety 12h/7 validators | SKIP | missing | FAIL |
| AC2 | determinism 50M-action week | SKIP | missing | FAIL |
| AC3 | throughput 100k/s, block<1s | SKIP | missing | FAIL |
| AC4 | clearinghouse invariants | SKIP | missing | FAIL |
| AC5 | SDK compatibility | SKIP | missing | FAIL |
| AC6 | bridge e2e + foundry | SKIP | missing | FAIL |
