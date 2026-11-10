# Interface requests (consensus)

None of these block the implementation; current workarounds are noted.

## 1. Document voter-side use of `ConsensusEnv::proposal_override` (doc only)
The core calls `proposal_override(h)` on voters too, to enforce that the
leader proposed exactly the logged block, and skips the `now + 2 s` drift
check for such heights (replay timestamps are sim time). Suggested doc diff in
`crates/interfaces/src/traits.rs`:
```diff
-    /// Replay mode: if Some, the leader must propose exactly this
-    /// (timestamp, payload) for `height` instead of mempool contents.
+    /// Replay mode: if Some, the leader must propose exactly this
+    /// (timestamp, payload) for `height` instead of mempool contents, and
+    /// voters refuse any other proposal for `height` (the drift check is
+    /// skipped). Must return the same value on every node; to commit the last
+    /// logged height, return padding blocks for 2 more heights.
```

## 2. (perf, optional) `Block::payload_hash` clones the payload
`Block::payload_hash(payload: &[Tx])` does `hash_borsh(&payload.to_vec())`
(a full clone) and `hash_borsh` feeds the hasher field-by-field (≈2.3× slower
than encoding into a buffer for 20k-tx blocks). Consensus uses its own
`crypto::payload_hash` (identical digest). Suggested:
```diff
     pub fn payload_hash(payload: &[Tx]) -> Hash32 {
-        hash_borsh(&payload.to_vec())
+        let mut buf = Vec::with_capacity(payload.len() * 128);
+        borsh::BorshSerialize::serialize(payload, &mut buf).expect("vec write");
+        blake3_hash(&buf)
     }
```

## 3. (perf, optional) `ConsensusOutput::Commit(Arc<CommittedBlock>)`
The core keeps blocks as `Arc<Block>`; the owned `CommittedBlock` in
`Commit` forces one full clone (~5 ms for 20k txs). An `Arc` there would
remove it. Not needed for the 5 blocks/s target.
