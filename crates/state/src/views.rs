//! Conversions from engine order types to API views.

use interfaces::*;

pub fn resting_view(o: &RestingOrder) -> RestingOrderView {
    RestingOrderView {
        asset: o.asset,
        oid: o.oid,
        side: o.side,
        limit_px: o.limit_px,
        sz: o.sz,
        orig_sz: o.orig_sz,
        timestamp: o.timestamp,
        cloid: o.cloid,
        reduce_only: o.reduce_only,
        order_type: "Limit".to_string(),
        tif: Some(o.tif),
        trigger: None,
        is_trigger: false,
    }
}

/// HL `orderType` strings for trigger orders.
pub fn trigger_order_type(tpsl: TpSl, is_market: bool) -> &'static str {
    match (tpsl, is_market) {
        (TpSl::Tp, true) => "Take Profit Market",
        (TpSl::Tp, false) => "Take Profit Limit",
        (TpSl::Sl, true) => "Stop Market",
        (TpSl::Sl, false) => "Stop Limit",
    }
}

pub fn trigger_view(t: &TriggerOrder) -> RestingOrderView {
    RestingOrderView {
        asset: t.asset,
        oid: t.oid,
        side: t.side,
        limit_px: t.limit_px,
        sz: t.sz,
        orig_sz: t.sz,
        timestamp: t.timestamp,
        cloid: t.cloid,
        reduce_only: t.reduce_only,
        order_type: trigger_order_type(t.tpsl, t.is_market).to_string(),
        tif: None,
        trigger: Some((t.trigger_px, t.is_market, t.tpsl)),
        is_trigger: true,
    }
}
