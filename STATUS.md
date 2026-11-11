# STATUS  (commit 86d06f0, 2026-11-11 18:08 UTC)

| kind | suite | passed | failed | result |
|---|---|---:|---:|---|
| build | cargo build --workspace | 1 | 0 | PASS |
| crate | interfaces | 3 | 0 | PASS |
| crate | consensus | 42 | 0 | PASS |
| crate | net | 17 | 0 | PASS |
| crate | mempool | 15 | 0 | PASS |
| crate | book | 38 | 0 | PASS |
| crate | clearinghouse | 23 | 0 | PASS |
| crate | oracle | 20 | 0 | PASS |
| crate | state | 46 | 0 | PASS |
| crate | hlwire | 34 | 0 | PASS |
| crate | api | 31 | 0 | PASS |
| crate | relayer | 26 | 0 | PASS |
| crate | sim | 16 | 0 | PASS |
| crate | node | 13 | 0 | PASS |
| crate | chaos | 38 | 0 | PASS |
| suite | forge (bridge) | 41 | 0 | PASS |
| suite | forge:test_ReplayProtection* | 1 | 0 | PASS |
| suite | forge:test_SignatureThreshold* | 1 | 0 | PASS |
| suite | forge:test_ValidatorSetRotation* | 1 | 0 | PASS |
| suite | no-float | 1 | 0 | PASS |

| AC | criterion | smoke | evidence | result |
|---|---|---|---|---|
| AC1 | consensus safety 12h/7 validators | ok | missing | FAIL |
| AC2 | determinism 50M-action week | ok | 50000002 actions / 100800 blocks / 7 sim-days; 2 runs x 4 validators + offline sim replay + gen roots compared per height; 0 mismatches; roots_digest 47eed582adf70013 | PASS |
| AC3 | throughput 100k/s, block<1s | ok | 110389 order actions/s over 120s (min 10s window 101089), median block 200.0 ms, 4 validators | PASS |
| AC4 | clearinghouse invariants | ok | proptests 4 tests / 40000 cases (0 failed); invariants every block of 100800 blocks (50000002 actions): offline sim replay + 8 node replays; 0 violations | PASS |
| AC5 | SDK compatibility | ok | python 15/15, ts 6/6 | PASS |
| AC6 | bridge e2e + foundry | ok | e2e ok; anvil delta 499 USDC; forge 41 passed / 0 failed | PASS |
