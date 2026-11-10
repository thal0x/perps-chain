# Interface requests from hlwire

None are blocking; hlwire works with interfaces rev 2 as is. Suggestions,
in priority order:

1. **`Action::SpotSend.amount: Usd` limits spot amounts to 6 decimals.**
   HL spot tokens have their own `weiDecimals` (PURR has 5, others up to 8+).
   With `Usd` an 8-decimal amount string is rejected by `decode_action`
   (exact parse, no rounding). If any in-scope token needs > 6 decimals,
   change to a token-agnostic fixed type, e.g.
   ```rust
   // tx.rs
   SpotSend { destination: Address, token: String, amount: Sz /* 1e-8 */, time: u64 },
   ```
   (hlwire would then parse with `parse_sz`).

2. **Order grouping `{ "p": n }` (TS SDK priority fee)** has no
   `Grouping` variant; hlwire decodes it as `Grouping::Na`. Only needed if
   priority fees are ever modeled:
   ```rust
   pub enum Grouping { Na, NormalTpsl, PositionTpsl, Priority(u64) }
   ```

3. **Doc fix** in `tx.rs`: it says "`hlwire::recover_signer`"; the function
   exists as `hlwire::recover_signer(&UserTx, is_mainnet)` and via
   `HlVerifier: TxVerifier`. No change needed, mentioned for clarity.
