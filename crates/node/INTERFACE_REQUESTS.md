# node interface requests

1. **In-flight tx exclusion in consensus** (consensus): `ConsensusEnv::payload` has no
   way to know which txs are in uncommitted ancestors of the block being proposed, so
   with pipelining + round-robin leaders the next leader re-proposes them. The node
   works around it by hashing every received proposal's payload (`inflight.rs`).
   Proposal: `fn payload(&mut self, max_txs, max_bytes, exclude: &[&Block])` (the
   uncommitted ancestors), or have the core pass the parent chain's tx hashes.
2. **Order-action limit at reap** (mempool/consensus): `max_block_order_actions` is not
   enforced at proposal time (reap only knows txs/bytes). Proposal: an optional
   `weight(&Tx) -> u32` callback in `Pool::reap` or a `reap_weighted`.
