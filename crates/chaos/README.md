# chaos — `hlchaos` fault-injection harness (TEST_PLAN AC1)

Drives real `hlnode` processes through the docs/NODE_CLI.md contract only
(`testnet-gen`, `run`, `commits.log`, `roots.log`, `node.log`,
`GET /admin/status`, `POST /admin/chaos`, `/exchange`, `/info`).

```
hlchaos run --validators 7 --duration 12h --max-faulty 2 --seed S \
            --hlnode PATH --workdir DIR [--load-rate 5] [--sim PATH] [--report out.json]
            [--episode-min 20s] [--episode-max 120s] [--observe 20s] [--resume-limit-ms 10000]
            [--stall-window 30s] [--kinds kill,isolate,...|all] [--node-arg=--min-block-ms --node-arg=200]
            [--allow-gaps] [--keep-workdir] [--no-verify-qcs]
hlchaos plan  --validators 7 --seed S --episodes 20       # print the seeded schedule (JSON lines)
hlchaos check DIR/net/node0 DIR/net/node1 ...             # offline safety check of node homes
scripts/chaos_smoke.sh                                    # release build + 3-min 7-validator run; exit 0 iff pass
```

Exit code: 0 pass, 1 fail, 2 harness error. Durations accept `12h`, `3m`,
`40s`, `500ms`, `1h30m`.

## What a run does
1. `hlnode testnet-gen` into `DIR/run-<seed>-<unix>/net` with free,
   seed-chosen ports (bind-probed); node addresses come from each
   `validator.key`, p2p addresses from `genesis.json`. Each node runs with
   explicit `--api-listen/--p2p-listen/--genesis`, `--log-level warn`
   unless overridden (bounded logs), stdout/stderr captured into
   `console.log` / `console.err.log` (rotated at 32 MiB, divergence lines
   counted by the capture thread so rotation never loses one).
2. Waits for all APIs and height ≥ 3, checks `/admin/chaos` works.
3. Load: `--sim PATH` spawns `PATH load --rps {rate} --target {target}`
   (template `--sim-args`); otherwise a built-in generator signs real HL
   actions (hlwire) with the funded `accounts.json` keys and posts them to a
   random live node: ALO bids 10–20 % below mid, then cancel-by-cloid
   (`noop` if no market is discoverable via `/info`). ≤ 32 requests in
   flight (`/exchange` answers after execution, so it blocks while halted).
4. Seeded **episodes** (20–120 s) then **heal** (`{}` to every node, killed
   nodes restarted) and an observation window:

   | kind | fault |
   |---|---|
   | `kill` | kill -9 1..f nodes (staggered), restart mid-episode or at heal |
   | `rolling_kill` | one node down at a time, cycling |
   | `isolate` | 1..f nodes each in its own partition group |
   | `minority_partition` | n-k \| k (minority together or split), k ≤ f |
   | `lossy` | 1..f nodes: delay 0–2000 ms, drop 0–30 %, reorder, duplicate (both directions: own outbound + peers' `links` to it), optional benign jitter |
   | `jitter` | network-wide benign jitter (≤ 100 ms, reorder, duplicate, no drop) |
   | `combo` | kill 1 + isolate/lossy f-1 others (+ jitter) |
   | `split` | whole-network split with no quorum group (3\|4, 2\|2\|3, 3\|3\|1, 2\|2 …) — progress must halt, then resume |

   **Invariant**: faulty = killed ∪ lossy ∪ nodes outside the quorum
   group; `|faulty| ≤ min(max_faulty, n - quorum)` at every instant except
   `split` episodes (which contain nothing else). Checked by
   `schedule::check_episode` (proptest over seeds, n = 4..7) and re-checked
   at runtime before every step. Jitter counts as non-faulty only because it
   is bounded to ≤ 100 ms without drops.
5. Report flushed every 30 s and after each episode (`complete: false`), so a
   crashed 12 h run still leaves evidence. At the end nodes get SIGTERM (then
   SIGKILL), commit proofs are verified, the final report is written, and
   `run-*/net` is **deleted** (kept with `--keep-workdir`; on failure the
   last 2 MiB of each node's logs are copied to `run-*/failure-logs/`).

## Checks
* **Safety (continuous, incremental tail every 500 ms)**: per height all
  nodes agree on `block_id` (`commits.log`) and on the root (`roots.log`);
  per node heights are contiguous (gaps are violations unless
  `--allow-gaps`, e.g. if snapshot sync ever skips heights); a re-logged
  identical height is fine, a different one is a conflict; any
  `STATE DIVERGENCE` in node.log/console is a violation. Torn last lines
  after kill -9 are dropped and the tail rewinds (hlnode truncates them on
  restart). During a quorumless split, any advance of the max committed
  height after a 5 s grace is a `progress_without_quorum` violation.
* **QC verification (post-run)**: each node's FileStore (`<home>/data`) is
  opened and the retained committed run (≤ 20 000 heights) is verified with
  `consensus::CertCtx::verify_committed_run` (≤ 4 unproven tip blocks
  allowed); every block id must match `commits.log`.
* **Liveness**: after each heal, `resume_ms` = time until the max committed
  height (status polls every 100 ms ∪ commits.log) is ≥ base + 2; fail if any
  > `--resume-limit-ms` (10 000) or never. `catchup_ms` (all nodes within 3
  blocks of the max) is recorded too. In non-halting phases a window of
  `--stall-window` (30 s) without any commit is a stall (fail).
* An unexpected node exit is counted (fail) and the node is restarted.

`pass` = no safety violations, ≥ 1 heal, all heals resumed within the
limit, no stalls, no unexpected exits, no harness errors, not interrupted.

## Report (`--report`, default `run-*/report.json`)
`{duration_s, validators, max_faulty, seed, episodes, heals, max_resume_ms,
p50_resume_ms, p99_resume_ms, resume_limit_ms, safety_violations, kills,
restarts, partitions, splits, lossy_episodes, progress_stalls,
unexpected_exits, final_height, load{..}, episodes_by_kind,
violations_by_kind, violations[≤200], checker{..}, qc{..}, heal_records[],
episode_records[], errors, complete, interrupted, pass, summary}`; stdout's
last line is the AC1 subset as one JSON object.

## Robustness
Children get `PR_SET_PDEATHSIG=SIGKILL` (die with the harness even on
SIGKILL); SIGINT/SIGTERM/SIGHUP stop the loop, heal nothing, stop nodes and
write the report (`interrupted: true`); `Drop` kills any survivors. HTTP is a
tiny blocking client with hard timeouts (one poller thread per node).

## Layout
`checker.rs` (safety), `schedule.rs` (episodes, invariant, ChaosConfig
JSON), `cluster.rs` (processes, ports, pollers, console capture),
`load.rs`, `runner.rs` (main loop, liveness), `qc.rs`, `report.rs`,
`tail.rs`, `http.rs`, `src/bin/fake_hlnode.rs` (test stand-in: commits
iff its partition group has a quorum; knobs `FAKE_RESUME_DELAY_MS`,
`FAKE_DIVERGE_NODE/HEIGHT`, `FAKE_CONFLICT_NODE/HEIGHT`).

## Tests
```
cargo test -p chaos          # ~45 s: unit + proptest + e2e runs against fake-hlnode
```
Unit: checker (conflicting commits, root mismatch, gaps, re-log after
restart, divergence lines, torn lines, pruning), scheduler (determinism,
kind coverage, invariant checker, proptest: max-faulty invariant / splits
quorumless / fault bounds), ChaosConfig JSON shapes, http, tail, load
(signed `/exchange` bodies parse with hlwire), console rotation, report.
E2E (`tests/e2e.rs`): mixed 7-validator run passes; split resume measured
(1.5 s injected delay); slow resume fails liveness; divergence + conflict
fail safety; startup failure; `plan`.

Manual runs against the real `hlnode` (debug build from agent/node):
4 validators 90 s and 7 validators 3 min (all kinds incl. a 3|3|1 split and
double kill) passed with max resume ≈ 1.1 s and QC verification of every
FileStore.
