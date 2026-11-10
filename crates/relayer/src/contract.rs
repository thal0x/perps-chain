//! Typed wrappers around Bridge.sol / MockUSDC calls and events.

use crate::abi::{self, Token};
use crate::rpc::{EthRpc, Log};
use anyhow::{ensure, Context, Result};
use interfaces::{Address, Hash32, Sig65};

pub const SIG_DEPOSIT_EVENT: &str = "Deposit(uint64,address,uint256)";
pub const SIG_WITHDRAWN_EVENT: &str = "Withdrawn(uint64,address,uint256)";
pub const SIG_WITHDRAW: &str = "withdraw((uint64,address,uint256),(uint8,bytes32,bytes32)[])";
pub const SIG_UPDATE_VALSET: &str = "updateValidatorSet(address[],uint256[],uint64,(uint8,bytes32,bytes32)[])";
pub const SIG_HASH_WITHDRAWAL: &str = "hashWithdrawal((uint64,address,uint256))";
pub const SIG_HASH_VALSET: &str = "hashValidatorSetUpdate(address[],uint256[],uint64)";

pub fn deposit_topic() -> Hash32 {
    abi::event_topic(SIG_DEPOSIT_EVENT)
}
pub fn withdrawn_topic() -> Hash32 {
    abi::event_topic(SIG_WITHDRAWN_EVENT)
}

/// Decoded `Deposit(uint64 indexed id, address indexed user, uint256 amount)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositEvent {
    pub id: u64,
    pub user: Address,
    /// USDC base units (6 decimals).
    pub amount: u128,
    pub tx_hash: Hash32,
    pub block_number: u64,
}

/// Decoded `Withdrawn(uint64 indexed id, address indexed destination, uint256 amount)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawnEvent {
    pub id: u64,
    pub destination: Address,
    pub amount: u128,
    pub tx_hash: Hash32,
    pub block_number: u64,
}

fn indexed_word(log: &Log, i: usize) -> Result<abi::Word> {
    log.topics.get(i).map(|t| t.0).with_context(|| format!("log missing topic {i}"))
}

pub fn decode_deposit(log: &Log) -> Result<DepositEvent> {
    ensure!(log.topics.first() == Some(&deposit_topic()), "not a Deposit log");
    Ok(DepositEvent {
        id: abi::u64_from_word(&indexed_word(log, 1)?)?,
        user: abi::address_from_word(&indexed_word(log, 2)?)?,
        amount: abi::u128_from_word(&abi::word_at(&log.data, 0)?)?,
        tx_hash: log.tx_hash,
        block_number: log.block_number,
    })
}

pub fn decode_withdrawn(log: &Log) -> Result<WithdrawnEvent> {
    ensure!(log.topics.first() == Some(&withdrawn_topic()), "not a Withdrawn log");
    Ok(WithdrawnEvent {
        id: abi::u64_from_word(&indexed_word(log, 1)?)?,
        destination: abi::address_from_word(&indexed_word(log, 2)?)?,
        amount: abi::u128_from_word(&abi::word_at(&log.data, 0)?)?,
        tx_hash: log.tx_hash,
        block_number: log.block_number,
    })
}

fn sig_token(s: &Sig65) -> Token {
    let v = crate::eip712::normalize_v(s.v).unwrap_or(s.v);
    Token::Tuple(vec![Token::u64(v as u64), Token::bytes32(s.r), Token::bytes32(s.s)])
}

fn withdrawal_token(id: u64, destination: &Address, amount: u128) -> Token {
    Token::Tuple(vec![Token::u64(id), Token::address(destination), Token::u128(amount)])
}

/// Calldata for `withdraw(w, sigs)`; `sigs` must already be sorted by signer.
pub fn withdraw_calldata(id: u64, destination: &Address, amount: u128, sigs: &[Sig65]) -> Vec<u8> {
    abi::encode_call(
        SIG_WITHDRAW,
        &[withdrawal_token(id, destination, amount), Token::Array(sigs.iter().map(sig_token).collect())],
    )
}

pub fn update_validator_set_calldata(validators: &[Address], powers: &[u128], epoch: u64, sigs: &[Sig65]) -> Vec<u8> {
    abi::encode_call(
        SIG_UPDATE_VALSET,
        &[
            Token::Array(validators.iter().map(Token::address).collect()),
            Token::Array(powers.iter().map(|p| Token::u128(*p)).collect()),
            Token::u64(epoch),
            Token::Array(sigs.iter().map(sig_token).collect()),
        ],
    )
}

pub fn deposit_calldata(amount: u128) -> Vec<u8> {
    abi::encode_call("deposit(uint256)", &[Token::u128(amount)])
}

pub fn approve_calldata(spender: &Address, amount: u128) -> Vec<u8> {
    abi::encode_call("approve(address,uint256)", &[Token::address(spender), Token::u128(amount)])
}

pub fn mint_calldata(to: &Address, amount: u128) -> Vec<u8> {
    abi::encode_call("mint(address,uint256)", &[Token::address(to), Token::u128(amount)])
}

/// Read-only views of a deployed Bridge.
pub struct BridgeView<'a> {
    pub rpc: &'a EthRpc,
    pub address: Address,
}

impl<'a> BridgeView<'a> {
    pub fn new(rpc: &'a EthRpc, address: Address) -> Self {
        Self { rpc, address }
    }

    async fn call(&self, data: Vec<u8>) -> Result<Vec<u8>> {
        self.rpc.call(&self.address, &data, None).await
    }

    async fn call_word(&self, sig: &str, args: &[Token]) -> Result<abi::Word> {
        let out = self.call(abi::encode_call(sig, args)).await?;
        abi::word_at(&out, 0).with_context(|| format!("{sig}: short return (is the bridge deployed?)"))
    }

    /// (validators, powers) of the current set, in contract order (ascending).
    pub async fn validators(&self) -> Result<(Vec<Address>, Vec<u128>)> {
        let out = self.call(abi::encode_call("getValidators()", &[])).await?;
        let vs = abi::decode_word_array(&out, 0)?.iter().map(abi::address_from_word).collect::<Result<Vec<_>>>()?;
        let ps = abi::decode_word_array(&out, 1)?.iter().map(abi::u128_from_word).collect::<Result<Vec<_>>>()?;
        ensure!(vs.len() == ps.len(), "getValidators: length mismatch");
        Ok((vs, ps))
    }

    pub async fn quorum_power(&self) -> Result<u128> {
        abi::u128_from_word(&self.call_word("quorumPower()", &[]).await?)
    }

    pub async fn epoch(&self) -> Result<u64> {
        abi::u64_from_word(&self.call_word("epoch()", &[]).await?)
    }

    pub async fn used_withdrawal(&self, id: u64) -> Result<bool> {
        abi::bool_from_word(&self.call_word("usedWithdrawal(uint64)", &[Token::u64(id)]).await?)
    }

    pub async fn token(&self) -> Result<Address> {
        abi::address_from_word(&self.call_word("token()", &[]).await?)
    }

    pub async fn min_deposit(&self) -> Result<u128> {
        abi::u128_from_word(&self.call_word("minDeposit()", &[]).await?)
    }

    pub async fn next_deposit_id(&self) -> Result<u64> {
        abi::u64_from_word(&self.call_word("nextDepositId()", &[]).await?)
    }

    pub async fn hash_withdrawal(&self, id: u64, destination: &Address, amount: u128) -> Result<[u8; 32]> {
        self.call_word(SIG_HASH_WITHDRAWAL, &[withdrawal_token(id, destination, amount)]).await
    }

    pub async fn hash_validator_set_update(&self, validators: &[Address], powers: &[u128], epoch: u64) -> Result<[u8; 32]> {
        self.call_word(
            SIG_HASH_VALSET,
            &[
                Token::Array(validators.iter().map(Token::address).collect()),
                Token::Array(powers.iter().map(|p| Token::u128(*p)).collect()),
                Token::u64(epoch),
            ],
        )
        .await
    }
}

/// ERC20 `balanceOf`.
pub async fn erc20_balance(rpc: &EthRpc, token: &Address, owner: &Address) -> Result<u128> {
    let out = rpc.call(token, &abi::encode_call("balanceOf(address)", &[Token::address(owner)]), None).await?;
    abi::u128_from_word(&abi::word_at(&out, 0)?)
}
