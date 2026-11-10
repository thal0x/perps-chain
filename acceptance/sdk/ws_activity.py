"""AC5 fixture: account activity while a long-running WS example is subscribed.

basic_ws.py / basic_adding.py subscribe to user channels (userEvents, userFills,
orderUpdates, userNonFundingLedgerUpdates, ...) that only emit when the account
does something. On real HL the example account would be trading; here this
driver makes that happen once, from the same account, through the SDK:
  1. market_open: buy 0.01 ETH (IoC, fills against the seeded MM ladder)
  2. market_close ETH
  3. usd_class_transfer 1 USDC perp->spot, then spot->perp (ledger updates)
It prints every response and exits non-zero if any call errors.

usage: python ws_activity.py <config.json> <base_url>
"""
import json
import sys

import eth_account

from hyperliquid.exchange import Exchange

cfg = json.load(open(sys.argv[1]))
url = sys.argv[2]
acct = eth_account.Account.from_key(cfg["secret_key"])
ex = Exchange(acct, url, account_address=cfg["account_address"])

bad = []


def check(name, r):
    print(name, json.dumps(r), flush=True)
    if not isinstance(r, dict) or r.get("status") != "ok":
        bad.append(name)
        return
    data = r.get("response", {}).get("data", {}) if isinstance(r.get("response"), dict) else {}
    for s in data.get("statuses", []) if isinstance(data, dict) else []:
        if isinstance(s, dict) and "error" in s:
            bad.append(name)


check("market_open", ex.market_open("ETH", True, 0.01, None, 0.01))
check("market_close", ex.market_close("ETH"))
check("usd_class_transfer_to_spot", ex.usd_class_transfer(1, False))
check("usd_class_transfer_to_perp", ex.usd_class_transfer(1, True))
if bad:
    print("ws_activity: failed calls:", bad, flush=True)
    sys.exit(1)
