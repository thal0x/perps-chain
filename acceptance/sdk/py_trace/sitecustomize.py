# AC5 observation hook (not part of the examples; the examples stay verbatim).
#
# Put on PYTHONPATH by acceptance/sdk/run_python.sh. When HL_ACC_TRACE is set,
# wraps hyperliquid.api.API.post so every HTTP request/response the SDK makes is
# appended as one JSON line to that file:
#   {"call": <outermost SDK method the example called>, "path": "/exchange",
#    "request": <payload>, "response": <parsed json>, "t_ms": ...}
# It only observes: arguments and return values are passed through unchanged.
import os

_TRACE = os.environ.get("HL_ACC_TRACE")

if _TRACE:
    import json
    import sys
    import time

    try:
        import hyperliquid
        import hyperliquid.api as _api

        _PKG = os.path.dirname(os.path.abspath(hyperliquid.__file__))
        _orig_post = _api.API.post

        def _outer_sdk_call():
            # Walk outwards from API.post; the answer is the last SDK frame before
            # the first non-SDK (example) frame, i.e. the method the example called.
            f = sys._getframe(2)
            name = None
            while f is not None:
                fn = os.path.abspath(f.f_code.co_filename)
                if not fn.startswith(_PKG + os.sep):
                    break
                cls = type(f.f_locals["self"]).__name__ if "self" in f.f_locals else ""
                name = f"{cls}.{f.f_code.co_name}" if cls else f.f_code.co_name
                f = f.f_back
            return name

        def _traced_post(self, url_path, payload=None):
            rec = {"call": _outer_sdk_call(), "path": url_path, "request": payload, "t_ms": int(time.time() * 1000)}
            try:
                resp = _orig_post(self, url_path, payload)
            except Exception as e:  # recorded, then re-raised unchanged
                rec["exception"] = f"{type(e).__name__}: {e}"
                _write(rec)
                raise
            rec["response"] = resp
            _write(rec)
            return resp

        def _write(rec):
            try:
                with open(_TRACE, "a") as fh:
                    fh.write(json.dumps(rec, default=str) + "\n")
            except Exception:
                pass

        _api.API.post = _traced_post

        # WebSocket: record each subscription the SDK sends and the identifier of
        # each message it receives (using the SDK's own routing function).
        import hyperliquid.websocket_manager as _wsm

        _orig_on_message = _wsm.WebsocketManager.on_message
        _orig_subscribe = _wsm.WebsocketManager.subscribe

        def _traced_on_message(self, _ws, message):
            rec = {"ws_msg": None, "t_ms": int(time.time() * 1000)}
            try:
                if message != "Websocket connection established.":
                    m = json.loads(message)
                    rec["channel"] = m.get("channel")
                    rec["ws_msg"] = _wsm.ws_msg_to_identifier(m)
                    rec["dispatched"] = bool(rec["ws_msg"]) and len(self.active_subscriptions.get(rec["ws_msg"], [])) > 0
                    if rec["channel"] == "trades" and isinstance(m.get("data"), list):
                        rec["n"] = len(m["data"])
            except Exception as e:
                rec["parse_error"] = str(e)
            _write(rec)
            return _orig_on_message(self, _ws, message)

        def _traced_subscribe(self, subscription, callback, subscription_id=None):
            ready = self.ws_ready
            r = _orig_subscribe(self, subscription, callback, subscription_id)
            if ready:
                _write({"ws_sub": _wsm.subscription_to_identifier(subscription), "subscription": subscription,
                        "t_ms": int(time.time() * 1000)})
            return r

        _wsm.WebsocketManager.on_message = _traced_on_message
        _wsm.WebsocketManager.subscribe = _traced_subscribe
    except Exception as e:  # never break the example because of the hook
        sys.stderr.write(f"[acc-trace] hook not installed: {e}\n")
