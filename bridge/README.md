# bridge/ — hlbft USDC bridge (Foundry)

Contracts (solc 0.8.28 pinned in `foundry.toml`, `forge-std` vendored in `lib/forge-std`, see DECISIONS.md D12):

* `src/MockUSDC.sol`: minimal ERC20 with 6 decimals and an open `mint(to, amount)`. Test only.
* `src/Bridge.sol`: the bridge (~250 lines, no dependencies, no admin, no pause).

## Bridge.sol

| function | what it does |
|---|---|
| `deposit(uint256 amount)` | pulls `amount` USDC (needs `approve`), assigns `id = nextDepositId++` (**first id is 0**), emits `Deposit(uint64 indexed id, address indexed user, uint256 amount)`. Reverts `DepositTooSmall` if `amount < minDeposit` and `DepositTooLarge` if `amount > 2^63-1`, the L1 `Usd` range. |
| `withdraw(Withdrawal{uint64 id; address destination; uint256 amount}, Sig{uint8 v; bytes32 r; bytes32 s}[] sigs)` | checks the signatures over the EIP-712 `Withdrawal` (see below), sets `usedWithdrawal[id]`, transfers USDC, emits `Withdrawn(uint64 indexed id, address indexed destination, uint256 amount)`. Anyone can call it. |
| `updateValidatorSet(address[] newValidators, uint256[] newPowers, uint64 newEpoch, Sig[] sigs)` | replaces the set. It must be signed by the **current** set at quorum, `newEpoch > epoch` must hold, and the new set must be strictly ascending, non-empty, at most 64 members, with powers in 1..2^64-1. Emits `ValidatorSetUpdated(epoch, validators, powers)`. |
| views | `hashWithdrawal(Withdrawal)`, `hashValidatorSetUpdate(address[],uint256[],uint64)`, `domainSeparator()`, `getValidators()`, `quorumPower()`, `totalPower()`, `powerOf(addr)`, `epoch()`, `usedWithdrawal(id)`, `nextDepositId()`, `token()`, `minDeposit()` |

### Signed messages (EIP-712)

```
domain   = EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)
           name "HlbftBridge", version "1", chainId = block.chainid, verifyingContract = bridge
Withdrawal(uint64 id,address destination,uint256 amount)
ValidatorSetUpdate(bytes32 validatorsHash,bytes32 powersHash,uint64 epoch)
    validatorsHash = keccak256(abi.encodePacked(address[]))   // 32-byte padded elements
    powersHash     = keccak256(abi.encodePacked(uint256[]))
digest   = keccak256(0x1901 || domainSeparator || structHash)
```

The withdrawal id is the nonce. `amount` is the USDC amount (6 decimals) that
`destination` receives, so the L1 has already deducted the 1 USDC fee.
`crates/relayer::withdrawal_digest` produces the same digest. This is tested
against the deployed contract.

### Signature rules (withdraw and updateValidatorSet)

* Every sig has `v ∈ {27, 28}`, `r != 0`, `0 < s <= secp256k1n/2` (EIP-2 low-s, so it is not malleable), and `ecrecover != 0`. Otherwise the call reverts with `InvalidSignature`.
* Recovered signers must be **strictly ascending**, which also makes them unique. Otherwise the call reverts with `SignersNotSorted`.
* Every signer must be in the current set. Otherwise the call reverts with `NotValidator` (unknown signers are rejected, not skipped).
* `Σ power(signers) >= quorumPower = floor(2·totalPower/3) + 1`, the smallest integer strictly above 2/3. This is identical to the L1's `ValidatorSet::quorum_stake`. Otherwise the call reverts with `InsufficientPower(have, need)`.

Replay protection:
* `usedWithdrawal[id]` blocks reuse of an id. Even a different payload with the same id is rejected.
* A strictly increasing epoch blocks replays of old rotation messages.
* The domain binds the chain and the contract.
* The two typehashes keep withdrawal and rotation signatures apart.
* Withdrawals are always checked against the current set, so signatures from retired validators stop counting at rotation.

## Tests

```
cd bridge && forge test
```

There are 41 tests in `test/Bridge.t.sol`:

* `test_SignatureThreshold_*` (13 plus 2 fuzz tests):
  * an exact quorum passes, while exactly 2/3 fails;
  * the weighted 7-of-10 boundary;
  * below quorum or empty signature lists;
  * duplicate and unsorted signers;
  * a non-validator signer;
  * high-s (malleated) signatures, bad v (29, and 0/1), zero r/s, and r off the curve;
  * signatures over another payload;
  * an invalid destination or amount;
  * an under-funded bridge;
  * fuzz: random weighted sets and random subsets pass iff `3·have > 2·total`;
  * fuzz: garbage signatures.
* `test_ReplayProtection_*` (5 plus 1 fuzz test):
  * the same id, and the same id with a different payload;
  * independent ids;
  * signatures bound to the contract and the chain id;
  * a withdrawal signature cannot be used for a rotation;
  * fuzz test.
* `test_ValidatorSetRotation_*` (11 plus 1 fuzz test):
  * success, and the new threshold applies;
  * below threshold;
  * the new set cannot authorise itself;
  * old-set signatures are rejected after rotation;
  * the old set cannot rotate again;
  * exact replay of the epoch, and the epoch must increase;
  * A→B→A, then replay of the A→B message;
  * the signature binds the members, powers and epoch;
  * an invalid new set;
  * partial overlap, where continuing members still count;
  * fuzz over epochs.
* Deposit tests (7 plus 1 fuzz test): monotone ids and events, the minimum and maximum amounts, a missing approval or balance, constructor validation, and views.

## Deploy (local Anvil)

```
anvil --block-time 1 &            # block time recommended: watchers wait for confirmations
VALIDATORS=0xA..,0xB..,0xC..,0xD.. POWERS=1,1,1,1 bridge/deploy.sh
# -> prints and writes bridge/deployments/local.json:
# {"bridge":"0x…","usdc":"0x…","chainId":31337,"deployBlock":0,"epoch":1,
#  "minDeposit":5000000,"validators":[…sorted…],"powers":[…]}
```

Env (all optional):

| var | default |
|---|---|
| `RPC_URL` | `http://127.0.0.1:8545` |
| `DEPLOYER_KEY` | Anvil account 0 |
| `VALIDATORS` | Anvil accounts 1..4. **Set this to the L1 genesis validator addresses**, and set `POWERS` to their stakes. |
| `POWERS` | all `1` |
| `EPOCH` | `1` |
| `MIN_DEPOSIT` | `5000000` (5 USDC). Should match `genesis.bridge.min_deposit`. |
| `MINT_AMOUNT` | 1,000,000 USDC minted to each Anvil dev account 0..9. `0` skips minting. |
| `DEPLOY_NAME` | `local` |

On a fresh Anvil with the default deployer, the addresses are deterministic:
USDC `0x5FbDB2315678afecb367f032d93F642f64180aa3`, Bridge
`0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512`. `genesis.bridge.contract` must
equal the Bridge address and `genesis.bridge.eth_chain_id` must be 31337.
`deployments/*.json` is gitignored.
