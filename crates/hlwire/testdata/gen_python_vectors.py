#!/usr/bin/env python3
"""Generate golden HL wire vectors with the official Python SDK.

Usage (from a venv with `pip install -e hyperliquid-python-sdk`):

    python gen_python_vectors.py > python_vectors.json

Every vector is produced by calling the real `hyperliquid.exchange.Exchange`
method; `API.post` is patched to capture the JSON body instead of sending it,
`get_timestamp_ms` is patched for deterministic nonces and `secrets.token_hex`
for a deterministic agent key. For each captured request we record:

* `body_raw`   - the exact JSON string `requests` would POST (json.dumps)
* `msgpack_hex`- msgpack.packb(body["action"])
* `kind`       - "l1" or "user"
* `action_hash`- (l1) keccak(msgpack || nonce || vault || expires)
* `digest`     - EIP-712 digest that was signed
* `signer`     - address expected from signature recovery
"""

import json
import sys

import eth_account
import msgpack
from eth_account.messages import encode_typed_data
from eth_utils import keccak

import hyperliquid.api as hl_api
import hyperliquid.exchange as hl_exchange
from hyperliquid.exchange import Exchange
from hyperliquid.utils import signing
from hyperliquid.utils.constants import MAINNET_API_URL
from hyperliquid.utils.types import Cloid

USER_KEY = "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318"
AGENT_KEY = "0x" + "11" * 32
GENERATED_AGENT_KEY_HEX = "22" * 32
VAULT = "0x1719884eb866cb12b2287399b15f7db5e7d775ea"
DEST = "0x5e9ee1089755c3435139848e47e6635505d5a13a"
LOCAL_URL = "http://localhost:3001"

META = {
    "universe": [
        {"name": "BTC", "szDecimals": 5},
        {"name": "ETH", "szDecimals": 4},
        {"name": "SOL", "szDecimals": 2},
    ]
}
SPOT_META = {"universe": [], "tokens": []}

USER_SIGNED_TYPES = {
    "usdSend": (signing.USD_SEND_SIGN_TYPES, "HyperliquidTransaction:UsdSend"),
    "spotSend": (signing.SPOT_TRANSFER_SIGN_TYPES, "HyperliquidTransaction:SpotSend"),
    "withdraw3": (signing.WITHDRAW_SIGN_TYPES, "HyperliquidTransaction:Withdraw"),
    "usdClassTransfer": (signing.USD_CLASS_TRANSFER_SIGN_TYPES, "HyperliquidTransaction:UsdClassTransfer"),
    "approveAgent": (
        [
            {"name": "hyperliquidChain", "type": "string"},
            {"name": "agentAddress", "type": "address"},
            {"name": "agentName", "type": "string"},
            {"name": "nonce", "type": "uint64"},
        ],
        "HyperliquidTransaction:ApproveAgent",
    ),
}

captured = []
_nonce = [1_700_000_000_000]


def fake_now():
    _nonce[0] += 1
    return _nonce[0]


def fake_post(self, url_path, payload=None):
    captured.append(payload)
    return {"status": "ok", "response": {"type": "default"}}


hl_api.API.post = fake_post
hl_exchange.get_timestamp_ms = fake_now
hl_exchange.secrets.token_hex = lambda n: GENERATED_AGENT_KEY_HEX[: 2 * n]


def make(key=USER_KEY, mainnet=False, vault=None, account=None):
    w = eth_account.Account.from_key(key)
    return Exchange(
        w,
        MAINNET_API_URL if mainnet else LOCAL_URL,
        meta=META,
        vault_address=vault,
        account_address=account,
        spot_meta=SPOT_META,
    )


vectors = []


def record(name, signer_key, mainnet, fn):
    captured.clear()
    fn()
    assert len(captured) == 1, name
    body = captured[0]
    body_raw = json.dumps(body)  # what requests(json=...) sends
    action = body["action"]
    nonce = body["nonce"]
    vault = body.get("vaultAddress")
    expires = body.get("expiresAfter")
    t = action["type"]
    v = {
        "name": name,
        "sdk": "python",
        "is_mainnet": mainnet,
        "signer": eth_account.Account.from_key(signer_key).address.lower(),
        "body_raw": body_raw,
        "body": body,
        "msgpack_hex": msgpack.packb(action).hex(),
    }
    if t in USER_SIGNED_TYPES:
        types, primary = USER_SIGNED_TYPES[t]
        msg = dict(action)
        if "agentName" not in msg and t == "approveAgent":
            msg["agentName"] = ""
        data = signing.user_signed_payload(primary, types, msg)
        v["kind"] = "user"
        v["primary_type"] = primary
    else:
        h = signing.action_hash(action, vault, nonce, expires)
        data = signing.l1_payload(signing.construct_phantom_agent(h, mainnet))
        v["kind"] = "l1"
        v["action_hash"] = "0x" + h.hex()
    sm = encode_typed_data(full_message=data)
    digest = keccak(b"\x19" + sm.version + sm.header + sm.body)
    v["digest"] = "0x" + digest.hex()
    # sanity: python recovery agrees
    rec = eth_account.Account.recover_message(
        sm, vrs=[body["signature"]["v"], body["signature"]["r"], body["signature"]["s"]]
    )
    assert rec.lower() == v["signer"], (name, rec)
    vectors.append(v)


def gen():
    for mainnet in (False, True):
        sfx = "_mainnet" if mainnet else ""
        ex = make(mainnet=mainnet)
        record("order_limit_gtc" + sfx, USER_KEY, mainnet, lambda: ex.order("BTC", True, 0.02, 30000.5, {"limit": {"tif": "Gtc"}}))
        record("usd_send" + sfx, USER_KEY, mainnet, lambda: ex.usd_transfer(12.5, DEST))

    ex = make()
    record("order_limit_ioc_reduce", USER_KEY, False, lambda: ex.order("ETH", False, 1.2345, 1100.0, {"limit": {"tif": "Ioc"}}, reduce_only=True))
    record("order_limit_alo_cloid", USER_KEY, False, lambda: ex.order("SOL", True, 3, 0.0001234, {"limit": {"tif": "Alo"}}, cloid=Cloid.from_int(1)))
    record("order_cloid_upper", USER_KEY, False, lambda: ex.order("BTC", True, 0.001, 65000, {"limit": {"tif": "Gtc"}}, cloid=Cloid("0x00000000000000000000000000ABCDEF")))
    record("order_trigger_sl_market", USER_KEY, False, lambda: ex.order("BTC", False, 0.5, 29000, {"trigger": {"triggerPx": 29500.0, "isMarket": True, "tpsl": "sl"}}, reduce_only=True))
    record("order_trigger_tp_limit", USER_KEY, False, lambda: ex.order("ETH", True, 2, 1999.9, {"trigger": {"triggerPx": 2000, "isMarket": False, "tpsl": "tp"}}))
    record("order_builder", USER_KEY, False, lambda: ex.order("BTC", True, 0.01, 40000, {"limit": {"tif": "Gtc"}}, builder={"b": "0x8C967E73E7B15087C42A10D344CFF4C96D877F1D", "f": 10}))

    def bulk_tpsl():
        orders = [
            {"coin": "BTC", "is_buy": True, "sz": 0.1, "limit_px": 50000, "order_type": {"limit": {"tif": "Gtc"}}, "reduce_only": False},
            {"coin": "BTC", "is_buy": False, "sz": 0.1, "limit_px": 60000, "order_type": {"trigger": {"triggerPx": 60000, "isMarket": True, "tpsl": "tp"}}, "reduce_only": True},
            {"coin": "BTC", "is_buy": False, "sz": 0.1, "limit_px": 45000, "order_type": {"trigger": {"triggerPx": 45000, "isMarket": True, "tpsl": "sl"}}, "reduce_only": True},
        ]
        ex.bulk_orders(orders, grouping="normalTpsl")

    record("bulk_orders_normal_tpsl", USER_KEY, False, bulk_tpsl)

    def bulk_pos_tpsl():
        orders = [
            {"coin": "ETH", "is_buy": False, "sz": 0, "limit_px": 4000, "order_type": {"trigger": {"triggerPx": 4000, "isMarket": True, "tpsl": "tp"}}, "reduce_only": True},
        ]
        ex.bulk_orders(orders, grouping="positionTpsl")

    record("bulk_orders_position_tpsl", USER_KEY, False, bulk_pos_tpsl)

    def bulk50():
        orders = []
        for i in range(50):
            orders.append({"coin": "BTC", "is_buy": i % 2 == 0, "sz": 0.001 * (i + 1), "limit_px": 30000 + i, "order_type": {"limit": {"tif": "Alo"}}, "reduce_only": False, "cloid": Cloid.from_int(1000 + i)})
        ex.bulk_orders(orders)

    record("bulk_orders_50", USER_KEY, False, bulk50)
    record("cancel", USER_KEY, False, lambda: ex.cancel("BTC", 123456789))
    record("bulk_cancel", USER_KEY, False, lambda: ex.bulk_cancel([{"coin": "BTC", "oid": 1}, {"coin": "SOL", "oid": 2**40 + 7}]))
    record("cancel_by_cloid", USER_KEY, False, lambda: ex.cancel_by_cloid("ETH", Cloid.from_int(0xDEADBEEF)))
    record("modify_by_oid", USER_KEY, False, lambda: ex.modify_order(77, "BTC", True, 0.03, 31000, {"limit": {"tif": "Gtc"}}))
    record("modify_by_cloid", USER_KEY, False, lambda: ex.modify_order(Cloid.from_int(5), "BTC", False, 0.03, 31000, {"limit": {"tif": "Alo"}}, cloid=Cloid.from_int(6)))

    def bmod():
        ex.bulk_modify_orders_new([
            {"oid": 1, "order": {"coin": "BTC", "is_buy": True, "sz": 1, "limit_px": 10, "order_type": {"limit": {"tif": "Gtc"}}, "reduce_only": False, "cloid": None}},
            {"oid": Cloid.from_int(2), "order": {"coin": "ETH", "is_buy": False, "sz": 0.5, "limit_px": 3000.25, "order_type": {"limit": {"tif": "Gtc"}}, "reduce_only": False, "cloid": Cloid.from_int(2)}},
        ])

    record("batch_modify", USER_KEY, False, bmod)
    record("schedule_cancel_time", USER_KEY, False, lambda: ex.schedule_cancel(1_700_000_100_000))
    record("schedule_cancel_none", USER_KEY, False, lambda: ex.schedule_cancel(None))
    record("update_leverage_cross", USER_KEY, False, lambda: ex.update_leverage(20, "BTC", True))
    record("update_leverage_isolated", USER_KEY, False, lambda: ex.update_leverage(3, "SOL", False))
    record("update_isolated_margin_add", USER_KEY, False, lambda: ex.update_isolated_margin(12.5, "ETH"))
    record("update_isolated_margin_remove", USER_KEY, False, lambda: ex.update_isolated_margin(-3000.25, "ETH"))
    record("update_isolated_margin_big", USER_KEY, False, lambda: ex.update_isolated_margin(1_000_000.0, "ETH"))
    record("set_referrer", USER_KEY, False, lambda: ex.set_referrer("ASDFASDF"))
    record("noop", USER_KEY, False, lambda: ex.noop(1_700_000_555_555))
    record("usd_send_int_amount", USER_KEY, False, lambda: ex.usd_transfer(5, DEST.upper().replace("0X", "0x")))
    record("spot_send", USER_KEY, False, lambda: ex.spot_transfer(1.25, DEST, "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2"))
    record("spot_send_usdc", USER_KEY, False, lambda: ex.spot_transfer(100.0, DEST, "USDC:0x6d1e7cde53ba9467b783cb7c530ce054"))
    record("withdraw3", USER_KEY, False, lambda: ex.withdraw_from_bridge(2.0, DEST))
    record("usd_class_transfer_to_perp", USER_KEY, False, lambda: ex.usd_class_transfer(10.0, True))
    record("usd_class_transfer_to_spot", USER_KEY, False, lambda: ex.usd_class_transfer(0.5, False))
    record("approve_agent_named", USER_KEY, False, lambda: ex.approve_agent("bot1"))
    record("approve_agent_unnamed", USER_KEY, False, lambda: ex.approve_agent(None))
    record("approve_agent_mainnet", USER_KEY, True, lambda: make(mainnet=True).approve_agent("m"))

    # vault + expiresAfter variants
    exv = make(vault=VAULT)
    record("order_vault", USER_KEY, False, lambda: exv.order("BTC", True, 0.02, 30000, {"limit": {"tif": "Gtc"}}))
    record("cancel_vault", USER_KEY, False, lambda: exv.cancel("BTC", 9))
    record("usd_class_transfer_vault_suffix", USER_KEY, False, lambda: exv.usd_class_transfer(1.0, True))
    exe = make()
    exe.set_expires_after(1_800_000_000_000)
    record("order_expires", USER_KEY, False, lambda: exe.order("ETH", False, 0.1, 2500, {"limit": {"tif": "Ioc"}}))
    record("update_leverage_expires", USER_KEY, False, lambda: exe.update_leverage(5, "ETH", False))
    exve = make(vault=VAULT, mainnet=True)
    exve.set_expires_after(1_800_000_000_001)
    record("order_vault_expires_mainnet", USER_KEY, True, lambda: exve.order("SOL", True, 10, 150.5, {"limit": {"tif": "Gtc"}}))

    # agent signing: agent key signs L1 actions on behalf of the account
    user_addr = eth_account.Account.from_key(USER_KEY).address
    exa = make(key=AGENT_KEY, account=user_addr)
    record("agent_order", AGENT_KEY, False, lambda: exa.order("BTC", False, 0.05, 35000, {"limit": {"tif": "Gtc"}}))
    record("agent_cancel", AGENT_KEY, False, lambda: exa.cancel("BTC", 42))


gen()
json.dump(vectors, sys.stdout, indent=1)
sys.stdout.write("\n")
