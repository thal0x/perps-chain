# Interface requests (book)

## 1. `MatchDecision::StopTaker` doc: remainder is always canceled

`interfaces::components::MatchDecision::StopTaker` says the remaining taker
size "rests if GTC, otherwise canceled". When the hook returns `StopTaker`,
the book is still crossing at the taker's limit price (the candidate it was
just offered crosses by definition), so resting the remainder would leave a
**crossed book** and break price-time priority for every later order. Both
use cases named in the doc (taker out of margin, a reduce-only taker that
would flip) want the remainder canceled anyway.

`book::Book` therefore **always cancels the taker remainder on `StopTaker`**
(`taker_canceled = true`, `resting_sz = 0`), whatever the TIF. Proposed doc
change (no type change, no rev bump needed):

```diff
-    /// Stop matching the taker here; remaining taker size is handled per TIF
-    /// (rests if GTC, otherwise canceled). Used when the taker runs out of
-    /// margin or a reduce-only taker would flip.
+    /// Stop matching the taker here; the remaining taker size is canceled
+    /// (never rests, whatever the TIF, since it would cross the book).
+    /// Used when the taker runs out of margin or a reduce-only taker would flip.
     StopTaker,
```

## 2. (informational) partial `Fill` semantics

`Fill` with accepted size `a < candidate.sz` reduces both sides by `a` and
**matching continues**: the same maker (if it still has size) is offered
again with the new remaining sizes. A hook that wants to stop after a partial
accept returns `StopTaker` on the next offer. Suggest documenting this on
`MatchHook::on_match`.
