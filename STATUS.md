# STATUS  (commit 5a34f19, 2026-11-10 15:49 UTC)

| kind | suite | passed | failed | result |
|---|---|---:|---:|---|
| build | cargo build --workspace | 1 | 0 | PASS |
| crate | interfaces | 3 | 0 | PASS |
| crate | consensus | 38 | 0 | PASS |
| crate | net | 17 | 0 | PASS |
| crate | mempool | 15 | 0 | PASS |
| crate | book | 38 | 0 | PASS |
| crate | clearinghouse | 23 | 0 | PASS |
| crate | oracle | 20 | 0 | PASS |
| crate | state | 35 | 0 | PASS |
| crate | hlwire | 29 | 0 | PASS |
| crate | api | 29 | 0 | PASS |
| crate | relayer | 26 | 0 | PASS |
| crate | sim | 16 | 0 | PASS |
| crate | node | 7 | 0 | PASS |
| crate | chaos | 38 | 0 | PASS |
| suite | forge (bridge) | 41 | 0 | PASS |
| suite | forge:test_ReplayProtection* | 1 | 0 | PASS |
| suite | forge:test_SignatureThreshold* | 1 | 0 | PASS |
| suite | forge:test_ValidatorSetRotation* | 1 | 0 | PASS |
| suite | no-float | 1 | 0 | PASS |

| AC | criterion | smoke | evidence | result |
|---|---|---|---|---|
| AC1 | consensus safety 12h/7 validators | SKIP | missing | FAIL |
| AC2 | determinism 50M-action week | SKIP | missing | FAIL |
| AC3 | throughput 100k/s, block<1s | SKIP | missing | FAIL |
| AC4 | clearinghouse invariants | SKIP | missing | FAIL |
| AC5 | SDK compatibility | SKIP | thresholds not met python 14/15, ts 4/6; failing: py:basic_ws, ts:02_trading, ts:05_market_order | FAIL |
| AC6 | bridge e2e + foundry | SKIP | ran on e2c476e958d7:db263aa35d91:e37eff47bf16:b8f9c6b40cdf e2e ok; anvil delta 499 USDC; forge 41 passed / 0 failed | STALE |
