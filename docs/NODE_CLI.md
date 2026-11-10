# hlnode CLI contract (lead-owned; node + chaos + scripts rely on it)

```
hlnode testnet-gen --validators N --out DIR [--base-p2p 26600] [--base-api 3001]
                   [--accounts N_FUNDED] [--fund-sdk-examples] [--seed S] [--genesis-time-ms T]
   writes DIR/genesis.json, DIR/node<i>/{validator.key (hex secret), config.toml},
   DIR/accounts.json ([{address, secret}] funded test accounts, incl. the SDK example
   account when --fund-sdk-examples), DIR/validators.json.

hlnode run --home DIR/node<i> [--genesis PATH] [--config PATH]
   flags override config.toml:
   --p2p-listen HOST:PORT   --api-listen HOST:PORT   --key PATH
   --min-block-ms 200       --invariants off|every|<N>
   --oracle-feed synthetic:<seed>[:skew_bps=<i>]|fixed|file:<path>
   --bridge-rpc http://127.0.0.1:8545   (enables DepositWatcher if set)
   --replay-log PATH        (leader proposes logged blocks, D8)
   --log-level info
```

Files written under `--home`:
* `commits.log`  — one line per committed block: `<height> <block_id_hex> <timestamp_ms>`, appended (fsync not required) at commit time.
* `roots.log`    — one line per executed block: `<height> <state_root_hex> <order_actions>`.
* `node.log`     — tracing output; contains `STATE DIVERGENCE` on root mismatch.
* `data/`        — consensus FileStore + snapshots (survives kill -9; restart resumes).

HTTP (on `--api-listen`): HL `/info`, `/exchange`, `/ws`, plus
`GET /admin/status` -> `interfaces::NodeStatus` JSON and
`POST /admin/chaos` -> body = `net::ChaosConfig` JSON (`{}` = heal).

Exit codes: 0 clean shutdown (SIGTERM/SIGINT), non-zero on fatal error.
