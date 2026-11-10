# HL error texts (for state/clearinghouse authors)

The api passes receipt error strings through verbatim. Use these where they
apply, so that SDK users see familiar messages.

Per-order (`OrderStatusResp::Error`):
* `Order has invalid price.`
* `Order has invalid size.`
* `Order must have minimum value of $10.`
* `Insufficient margin to place order.`
* `Reduce only order would increase position.`
* `Post only order would have immediately matched, bbo was {px}.`
* `Order could not immediately match against any resting orders.` (IOC no fill)
* `Invalid TP/SL price.`
* `Order price cannot be more than 95% away from the reference price`

Cancel (`SimpleStatus::Error`):
* `Order was never placed, already canceled, or filled.`

Whole tx (`Err(..)`, rendered as `{"status":"err","response":..}`):
* `User or API Wallet 0x... does not exist.` (unknown signer; the api uses this for `AdmissionError::UnknownSigner`)
* `Invalid nonce: duplicate nonce` / `Invalid nonce: nonce too old` / `Invalid nonce: nonce too far in future`
* `Insufficient balance for withdrawal` / `Insufficient balance for transfer`
* `Cannot modify canceled or filled order`
* `Scheduled cancel time too early, must be at least 5 seconds after current time`
* `Agent address is already approved` (HL is lenient; optional)
