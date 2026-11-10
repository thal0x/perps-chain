"""Drive the official hyperliquid-python-sdk against an hlbft API server.

Usage: python sdk_smoke.py <base_url> <account_address>
Signatures are produced by the SDK; the test backend does not verify them.
"""
import os
import sys
import threading
import time

from eth_account import Account

from hyperliquid.exchange import Exchange
from hyperliquid.info import Info
from hyperliquid.utils.types import Cloid

import traceback

base, addr = sys.argv[1], sys.argv[2]


def main():


    def ok(r, what):
        assert isinstance(r, dict) and r.get("status") == "ok", f"{what}: {r}"
        return r


    info = Info(base, skip_ws=False)
    us = info.user_state(addr)
    assert float(us["marginSummary"]["accountValue"]) > 0
    assert us["assetPositions"][0]["position"]["coin"] == "ETH"
    assert len(info.spot_user_state(addr)["balances"]) >= 1
    assert any(o["coin"] == "ETH" for o in info.open_orders(addr))
    info.frontend_open_orders(addr)
    assert float(info.all_mids()["ETH"]) > 0
    book = info.l2_snapshot("ETH")
    assert float(book["levels"][0][0]["px"]) > 0
    info.user_fills(addr)
    info.user_fills_by_time(addr, 0)
    info.meta()
    info.meta_and_asset_ctxs()
    info.spot_meta_and_asset_ctxs()
    info.funding_history("ETH", 0)
    info.user_funding_history(addr, 0)
    info.candles_snapshot("ETH", "1m", 0, int(time.time() * 1000))
    info.user_fees(addr)
    assert info.query_order_by_oid(addr, 10)["status"] == "order"
    assert info.query_order_by_cloid(addr, Cloid.from_int(1))["status"] == "order"
    assert info.query_order_by_cloid(addr, Cloid.from_int(2))["status"] == "unknownOid"
    info.historical_orders(addr)
    info.user_non_funding_ledger_updates(addr, 0)
    info.user_rate_limit(addr)
    info.query_referral_state(addr)
    print("info ok")

    ex = Exchange(Account.create(), base, account_address=addr)
    r = ok(ex.order("ETH", True, 0.2, 1100, {"limit": {"tif": "Gtc"}}), "order")
    oid = r["response"]["data"]["statuses"][0]["resting"]["oid"]
    ok(ex.cancel("ETH", oid), "cancel")
    cloid = Cloid.from_str("0x00000000000000000000000000000001")
    ok(ex.order("ETH", True, 0.2, 1100, {"limit": {"tif": "Gtc"}}, cloid=cloid), "order cloid")
    ok(ex.cancel_by_cloid("ETH", cloid), "cancel by cloid")
    ok(ex.modify_order(oid, "ETH", True, 0.1, 1105, {"limit": {"tif": "Gtc"}}, cloid=cloid), "modify")
    ok(ex.market_open("ETH", False, 0.05, None, 0.01), "market open")
    ok(ex.market_close("ETH"), "market close")
    ok(ex.order("ETH", False, 0.02, 2500, {"trigger": {"triggerPx": 2600, "isMarket": True, "tpsl": "sl"}}, reduce_only=True), "sl")
    orders = [
        {"coin": "ETH", "is_buy": True, "sz": 0.02, "limit_px": 3500, "order_type": {"limit": {"tif": "Gtc"}}, "reduce_only": False},
        {"coin": "ETH", "is_buy": False, "sz": 0.02, "limit_px": 3500, "order_type": {"trigger": {"isMarket": True, "triggerPx": 3500, "tpsl": "tp"}}, "reduce_only": True},
    ]
    ok(ex.bulk_orders(orders, grouping="normalTpsl"), "bulk tpsl")
    ok(ex.update_leverage(21, "ETH"), "leverage cross")
    ok(ex.update_leverage(21, "ETH", False), "leverage isolated")
    ok(ex.update_isolated_margin(1, "ETH"), "isolated margin")
    ok(ex.usd_transfer(1, "0x0000000000000000000000000000000000000000"), "usd transfer")
    ok(ex.spot_transfer(1, "0x0000000000000000000000000000000000000000", "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2"), "spot transfer")
    ok(ex.usd_class_transfer(1.23, False), "class transfer")
    ok(ex.withdraw_from_bridge(2, addr), "withdraw")
    ok(ex.approve_agent()[0], "approve agent")
    ok(ex.schedule_cancel(int(time.time() * 1000) + 10000), "schedule cancel")
    print("exchange ok")

    # --- websocket: the subscriptions of examples/basic_ws.py
    got = {}
    lock = threading.Lock()


    def cb(name):
        def f(msg):
            with lock:
                got.setdefault(name, msg)

        return f


    subs = {
        "allMids": {"type": "allMids"},
        "l2Book": {"type": "l2Book", "coin": "ETH"},
        "tradesSpot": {"type": "trades", "coin": "PURR/USDC"},
        "trades": {"type": "trades", "coin": "ETH"},
        "userEvents": {"type": "userEvents", "user": addr},
        "userFills": {"type": "userFills", "user": addr},
        "candle": {"type": "candle", "coin": "ETH", "interval": "1m"},
        "orderUpdates": {"type": "orderUpdates", "user": addr},
        "userFundings": {"type": "userFundings", "user": addr},
        "userNonFundingLedgerUpdates": {"type": "userNonFundingLedgerUpdates", "user": addr},
        "webData2": {"type": "webData2", "user": addr},
        "bbo": {"type": "bbo", "coin": "ETH"},
        "activeAssetCtx": {"type": "activeAssetCtx", "coin": "BTC"},
        "activeSpotAssetCtx": {"type": "activeAssetCtx", "coin": "@1"},
        "activeAssetData": {"type": "activeAssetData", "user": addr, "coin": "BTC"},
    }
    for name, s in subs.items():
        info.subscribe(s, cb(name))
    need = set(subs) - {"tradesSpot"}
    deadline = time.time() + 20
    while time.time() < deadline:
        with lock:
            if need <= set(got):
                break
        time.sleep(0.1)
    with lock:
        missing = need - set(got)
    assert not missing, f"no websocket message for {missing}"
    assert got["userFills"]["data"]["isSnapshot"] is True
    assert got["activeSpotAssetCtx"]["channel"] == "activeSpotAssetCtx"
    print("ws ok")


try:
    main()
    code = 0
except BaseException:
    traceback.print_exc()
    code = 1
sys.stdout.flush()
sys.stderr.flush()
os._exit(code)  # the SDK's websocket thread is not a daemon
