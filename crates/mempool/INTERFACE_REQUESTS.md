# Interface requests (mempool) — non-blocking

## R1: let `check_tx` return the recovered signer

`Application::check_tx(&self, tx) -> Result<(), AdmissionError>` throws away
the signer it recovers, so per-signer rate limiting needs a second recovery
(~25 µs) or a side cache. Proposal:

```diff
-    fn check_tx(&self, tx: &Tx) -> Result<(), AdmissionError>;
+    /// Returns the recovered signer (principal or agent) on success.
+    fn check_tx(&self, tx: &Tx) -> Result<Option<Address>, AdmissionError>;
```

Meanwhile the pool accepts `PoolDeps::signer_of` or the `signer` argument of
`Pool::insert_prechecked`, so nothing is blocked.
