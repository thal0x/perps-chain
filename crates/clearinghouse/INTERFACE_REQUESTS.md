# Interface requests (clearinghouse)

None are blocking; the crate works against interfaces rev 2 with inherent
helper methods. Suggested for a future rev:

1. **Withdrawal fee in the trait.** `Clearinghouse::withdraw(user, amount)`
   has no fee; HL `withdraw3` debits `amount` and 1 USDC is a fee. The crate
   offers inherent `withdraw_with_fee(user, amount, fee)` (fee → fee pool,
   `amount − fee` leaves the system). Proposal:
   ```rust
   fn withdraw(&mut self, user: Address, amount: Usd, fee: Usd) -> Result<(), ExecError>;
   ```
2. **Post-liquidation check.** AC4 invariant 3 can only be checked after the
   pass; `check_invariants` is called at arbitrary points. The crate exposes
   inherent `negative_equity_accounts() -> Vec<Address>`. Proposal:
   ```rust
   fn check_after_liquidations(&self, height: Height) -> Result<(), InvariantViolation>;
   ```
3. **Genesis flows.** `ExternalFlows.genesis` must equal perp balances +
   spot USDC + insurance fund; the crate exposes
   `Clearinghouse::genesis_total(&Genesis)`. Could live in `interfaces`.
4. **Backstop/ADL fill ids.** `backstop_and_adl` emits `Event::Fill` with
   `tid = 0`, zero oids/tx hash (the clearinghouse has no tid counter). The
   state crate should renumber; alternatively pass a `&mut Tid` counter.
5. **Entity hashes after restore.** `take_dirty` reports changes only;
   after `decode` the state crate needs all hashes: inherent
   `all_entity_hashes()`. Could be a trait method.
