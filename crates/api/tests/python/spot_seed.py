"""PURR/USDC liquidity fixture for a localnet (D25), via the official Python SDK.

usage: python spot_seed.py <localnet dir with accounts.json> <base_url>

1. The sdk_example account (the only genesis PURR holder) sends 20000 PURR to
   market maker = funded test account #0; the MM moves 50000 USDC perp->spot.
2. The MM rests a 5-level ladder: bids 0.99..0.95, asks 1.01..1.05, 1000 PURR
   each (so basic_spot_order's 24 @ 0.5 rests, as on HL).
3. Test account #1 moves 100 USDC to spot and market-buys 20 PURR, so
   PURR/USDC has a trade: `trades` / `candle` / `recentTrades` are non-empty
   and basic_ws's `trades PURR/USDC` subscription gets its snapshot at once.
Exits non-zero if any call fails.
"""
import json
import os
import sys

import eth_account
from hyperliquid.exchange import Exchange

PURR = "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2"
d, url = sys.argv[1], sys.argv[2]
accts = json.load(open(os.path.join(d, "accounts.json")))
sdk = [a for a in accts if a.get("name") == "sdk_example"]
mm = [a for a in accts if not a.get("name")]
if not sdk or len(mm) < 2:
    print("spot seed: need the sdk_example account and two funded test accounts; skipping")
    sys.exit(0)
bad = []


def ok(name, r):
    good = isinstance(r, dict) and r.get("status") == "ok"
    if good:
        data = r.get("response", {}).get("data", {}) if isinstance(r.get("response"), dict) else {}
        good = not any(isinstance(s, dict) and "error" in s for s in (data.get("statuses", []) if isinstance(data, dict) else []))
    if not good:
        bad.append(name)
        print(f"spot seed: {name} failed: {json.dumps(r)}")
    return r


sdk_w = eth_account.Account.from_key(sdk[0]["secret"])
mm_w = eth_account.Account.from_key(mm[0]["secret"])
tk_w = eth_account.Account.from_key(mm[1]["secret"])
sdk_ex, mm_ex, tk_ex = Exchange(sdk_w, url), Exchange(mm_w, url), Exchange(tk_w, url)

ok("purr_to_mm", sdk_ex.spot_transfer(20000, mm_w.address, PURR))
ok("mm_usdc_to_spot", mm_ex.usd_class_transfer(50000, False))
ladder = []
for lvl in range(1, 6):
    for is_buy, px in ((True, round(1 - 0.01 * lvl, 2)), (False, round(1 + 0.01 * lvl, 2))):
        ladder.append({"coin": "PURR/USDC", "is_buy": is_buy, "sz": 1000, "limit_px": px,
                       "order_type": {"limit": {"tif": "Gtc"}}, "reduce_only": False})
r = ok("mm_ladder", mm_ex.bulk_orders(ladder))
st = r.get("response", {}).get("data", {}).get("statuses", []) if isinstance(r, dict) else []
print(f"spot liquidity: {sum(1 for s in st if 'resting' in s)}/{len(ladder)} PURR/USDC orders resting from {mm_w.address}")
ok("taker_usdc_to_spot", tk_ex.usd_class_transfer(100, False))
ok("taker_buy", tk_ex.market_open("PURR/USDC", True, 20, None, 0.01))
sys.exit(1 if bad else 0)
