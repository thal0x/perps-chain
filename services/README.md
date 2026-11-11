# Localnet services: faucet, market maker, trader bots

These make a local hlbft network feel like a live testnet: the order books are
always full, prices tick, trades and the occasional liquidation happen, and
anyone can get test money from a faucet.

| piece | what it does |
|---|---|
| **faucet** | A small web service (default http://127.0.0.1:3100). `POST /drip {"address": "0x…"}` sends 10,000 test USDC (and 100 PURR, if the faucet holds some) to that address. `GET /status` shows the faucet's balance, its limits and the recent drips. The trading UI's **Get test funds** button calls it. |
| **market maker** (`mm`) | Keeps 5 buy orders and 5 sell orders on every perp market (BTC, ETH, SOL, AVAX, DOGE, ARB) and on PURR/USDC, a little below and above the current price, and moves them as the price moves. |
| **traders** | Four wallets that trade every 5 to 20 seconds, each with its own habit: `bull` mostly buys, `bear` mostly sells, `reverter` buys dips and sells rallies and often leaves limit orders on the book, `degen` bets small amounts at 50x leverage and never cuts a loss, so it gets liquidated now and then. |

### What a market maker is, and why the bots slowly lose money

On an exchange, a trade needs someone on the other side. A **market maker**
always offers to buy a bit below the current price and to sell a bit above it.
The gap between its best buy and best sell is the **spread**. Someone who wants
to trade right now (a "market order") buys at the market maker's higher price or
sells at its lower one. The market maker earns roughly the spread on each
round trip, and the impatient trader pays it, plus the exchange's trading fee.
Here the bots trade more or less at random, so on average they lose a little on
every trade, and the degen also loses its stake whenever it is liquidated. To
keep them going, each bot asks the faucet for a top-up whenever its free
balance falls below a threshold. The market maker's main risk is ending up
holding a big position when the price moves, so it caps its position per market
and shifts its prices to work that position back down.

## Start and stop

You need Node 22 (`node -v`) and a running localnet.

```bash
scripts/localnet.sh dev --bots     # localnet + seed + faucet + bots, all in one
scripts/localnet.sh stop           # stops the bots (cancelling the MM's orders) and the nodes
```

To start the services against a localnet that is already running:

```bash
cd services
npm ci          # first time only
npm start       # faucet + market maker + traders; Ctrl-C to stop
```

`npm start` reads `../run/localnet` (the API address comes from
`validators.json`, keys from `accounts.json`). Other entry points:
`npm run faucet` (faucet only), `npm run bots` (market maker and traders only,
using a faucet that is already running). `scripts/localnet.sh seed --bots`
does the same as `npm start`, but in the background, against the running localnet.

Ctrl-C (or `localnet.sh stop`, which sends SIGTERM) shuts down cleanly: the
market maker and the traders cancel their open orders first.

## Whose money is used

* **Faucet**: the last funded test account in `run/localnet/accounts.json`
  (with the default 10 accounts, test account #9; 1,000,000 USDC). The seeding
  script uses test accounts #0 and #1, so these never overlap.
* **Market maker**: the second-to-last test account (#8, 1,000,000 USDC). It
  moves 50,000 USDC to its spot balance for PURR/USDC buy orders.
* **PURR**: only the `sdk_example` account holds PURR at genesis. On first
  start the services move 20,000 PURR from it to the market maker (to sell on
  PURR/USDC) and 10,000 to the faucet (for drips). `sdk_example` keeps at least
  20,000. Turn this off with `--set bootstrapPurr=false`.
* **Traders**: fresh random keys, created on first start and kept in
  `run/localnet/bots/wallets.json` (readable only by you). They start empty and
  get all their money from the faucet, which is how the faucet gets exercised.
  `localnet.sh start`/`dev` wipe `run/localnet`, so a new localnet gets new
  bot wallets.

You can override the faucet or market maker key with `FAUCET_KEY=0x…` or
`MM_KEY=0x…`. Keys are never logged or served.

## Faucet limits

* one drip per address every 10 minutes (`faucet.addressCooldownMin`);
* at most 5 drips per client IP per 10 minutes (`faucet.ipMaxDrips`,
  `faucet.ipWindowMin`). The bots skip this limit by sending a token that the
  faucet writes to `run/localnet/services/faucet.token`; the per-address limit
  still applies to them;
* at most 500,000 USDC per 24 hours in total (`faucet.dailyCapUsdc`).

A refused request gets HTTP 429 with a message such as
`rate limited: this address got funds 40 s ago; try again in 10 min` and a
`retryAfterSec` field. The limits survive restarts (`faucet-state.json`).

```bash
curl -s -X POST localhost:3100/drip -H 'content-type: application/json' -d '{"address":"0xYourAddress"}'
curl -s localhost:3100/status
```

## Configuration

Defaults are meant to work as they are. To change something, use one of:

* a file `services/services.config.json` (only the keys you want to change), e.g.
  ```json
  { "traders": { "activity": 2 }, "mm": { "halfSpreadBps": 10 }, "faucet": { "port": 3200 } }
  ```
* environment variables for the common knobs (table below);
* `npm start -- --set mm.levels=8 --set traders.count=8` (any key, dotted path).

| knob | env | default | meaning |
|---|---|---|---|
| `localnetDir` | `LOCALNET_DIR` | `../run/localnet` | where `accounts.json` / `validators.json` are |
| `apiUrl` | `HL_API` | node0 of the localnet | node API base URL |
| `faucetUrl` | `FAUCET_URL` | `http://127.0.0.1:<faucet.port>` | faucet the bots call |
| `faucet.port` | `FAUCET_PORT` | 3100 | faucet port (`faucet.host` 127.0.0.1; use 0.0.0.0 to serve your LAN) |
| `faucet.usdcAmount` | `FAUCET_AMOUNT` | 10000 | USDC per drip |
| `faucet.purrAmount` | | 100 | PURR per drip (0 = none) |
| `faucet.addressCooldownMin` | `FAUCET_COOLDOWN_MIN` | 10 | minutes between drips to one address |
| `faucet.ipMaxDrips` / `ipWindowMin` | | 5 / 10 | per-IP limit |
| `faucet.dailyCapUsdc` | `FAUCET_DAILY_CAP` | 500000 | USDC per rolling 24 h |
| `traders.count` | `TRADERS` | 4 | number of trader wallets (habits repeat: bull, bear, reverter, degen, bull2, …) |
| `traders.activity` | `ACTIVITY` | 1 | pace: 1 = each wallet acts every 5–20 s; 2 = twice as often; 0.5 = half |
| `traders.topUpBelowUsd` | `TOPUP_BELOW` | 2000 | ask the faucet for more when free USDC is below this |
| `traders.spotShare` | | 0.12 | share of trader actions on PURR/USDC |
| `traders.perps` / `mm.perps` | | `"all"` | markets to trade / quote, e.g. `["BTC","ETH"]` |
| `mm.levels` | `MM_LEVELS` | 5 | orders per side |
| `mm.halfSpreadBps` | `MM_SPREAD_BPS` | 5 | distance of the best orders from the price, in basis points (5 bp = 0.05%) |
| `mm.levelStepBps` | | 5 | extra distance per level |
| `mm.levelUsd` / `levelSizeGrowth` | | 3000 / 1.5 | size of the first level in USD; each deeper level is 1.5× bigger |
| `mm.intervalMs` | | 1500 | how often it checks prices |
| `mm.requoteBps` | | 4 | only move orders when the price moved this much (or an order was filled) |
| `mm.maxPositionUsd` | `MM_MAX_POSITION` | 50000 | hard cap on the position per market |
| `mm.maxSkewBps` | | 25 | how far it shifts prices to shed a position at the cap |
| `mm.leverage` | | 3 | cross leverage |
| `mm.spotTargetPurr` / `spotMaxDeviationPurr` | | 20000 / 15000 | PURR it aims to hold, and how far it may drift |
| `mm.sweepAboveUsd` | | 0 (off) | send market-maker equity above this back to the faucet every 10 min |
| `bootstrapPurr` | | true | move PURR from `sdk_example` at first start (see above) |

## How the market maker quotes

Every 1.5 s it reads the oracle price of each perp, its position and its open
orders. It wants 5 bids at 5, 10, 15, 20 and 25 bp below the price and 5 asks
the same distance above, sized $3,000, $4,500, $6,750, … (rounded to the
market's size step, prices to 5 significant figures). If it is long, all
prices shift down (up to 25 bp at the cap) so it sells more readily than it
buys. The total size of its bids is limited to `cap − long position` (minus 3% headroom for price moves), so even
if all of them fill, the position stays within the cap; same for asks. It only
sends anything when the price moved at least 4 bp or one of its orders was
filled, and then moves its existing orders with one `batchModify` for all
markets (plus one order batch and one cancel batch if levels appear or go). At
3x leverage with a $50,000 cap per market, its worst-case margin use is about
$100,000 of a $1,000,000 account, so it does not get liquidated.

PURR/USDC has no oracle. There the market maker starts from the current mid
and lets its reference price drift in the direction its inventory pushes it, so
steady buying lifts the price and steady selling lowers it.

## Logs

* `run/localnet/services/mm.log`, `traders.log`, `faucet.log`: one line per action;
* `run/localnet/services/services.log`: startup, shutdown and per-bot totals;
* `run/localnet/services/stdout.log`: everything together, when started by `localnet.sh`.

With `npm start`, the same lines go to the terminal. The market maker prints a
status line every minute (equity, PnL since start, inventory per market).

## Files

`src/main.ts` (start/stop and PURR bootstrap), `src/faucet.ts`, `src/mm.ts`,
`src/trader.ts`, `src/config.ts` (all knobs and defaults), `src/common.ts`.
`npm run typecheck` runs `tsc --noEmit`.

## Notes

* The local chain signs as testnet (D9); the SDK is `@nktkas/hyperliquid`
  0.33.3, the same version as `frontend/`.
* The degen liquidates when the price moves about 1% against it (50x on BTC or
  ETH). With the localnet's synthetic oracle (about 80% annual volatility, a
  small jump about once an hour), that happens on the order of once or twice an
  hour, not every few minutes.
* `localnet.sh seed` rests a fixed ladder of large orders (test account #0) at
  the prices of the moment. After a sudden oracle move these stale orders can
  hold the book's mid away from the oracle until traders eat through them. The
  market maker trades against them up to its position cap and then waits, so
  for a while the UI can show mid/mark a fraction of a percent off the oracle.
  This fades over time, and it does not happen if you don't seed.
