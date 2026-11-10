//! Legacy (EIP-155) transaction signing with RLP, and a small `TxSender`
//! that fills nonce / gas price / gas limit from the node and waits for the
//! receipt.

use crate::abi::keccak256;
use crate::eip712::secret_address;
use crate::rpc::{EthRpc, Receipt};
use anyhow::{bail, Result};
use interfaces::{Address, Hash32};
use secp256k1::{Message, SecretKey, SECP256K1};
use std::time::Duration;

// ------------------------------------------------------------------ RLP ---
pub enum Rlp<'a> {
    Bytes(&'a [u8]),
    Uint(u128),
    List(Vec<Rlp<'a>>),
}

fn len_prefix(out: &mut Vec<u8>, len: usize, short: u8, long: u8) {
    if len <= 55 {
        out.push(short + len as u8);
    } else {
        let lb = (len as u64).to_be_bytes();
        let skip = lb.iter().take_while(|b| **b == 0).count();
        out.push(long + (8 - skip) as u8);
        out.extend_from_slice(&lb[skip..]);
    }
}

fn rlp_bytes(out: &mut Vec<u8>, b: &[u8]) {
    if b.len() == 1 && b[0] < 0x80 {
        out.push(b[0]);
    } else {
        len_prefix(out, b.len(), 0x80, 0xb7);
        out.extend_from_slice(b);
    }
}

/// Minimal big-endian bytes of an integer (empty for 0).
pub fn trim_be(b: &[u8]) -> &[u8] {
    let skip = b.iter().take_while(|x| **x == 0).count();
    &b[skip..]
}

pub fn rlp_encode(item: &Rlp<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    rlp_append(&mut out, item);
    out
}

fn rlp_append(out: &mut Vec<u8>, item: &Rlp<'_>) {
    match item {
        Rlp::Bytes(b) => rlp_bytes(out, b),
        Rlp::Uint(v) => rlp_bytes(out, trim_be(&v.to_be_bytes())),
        Rlp::List(items) => {
            let mut body = Vec::new();
            for i in items {
                rlp_append(&mut body, i);
            }
            len_prefix(out, body.len(), 0xc0, 0xf7);
            out.extend_from_slice(&body);
        }
    }
}

// ------------------------------------------------------------- legacy tx ---
#[derive(Clone, Debug)]
pub struct LegacyTx {
    pub nonce: u64,
    pub gas_price: u128,
    pub gas: u64,
    /// None = contract creation.
    pub to: Option<Address>,
    pub value: u128,
    pub data: Vec<u8>,
    pub chain_id: u64,
}

impl LegacyTx {
    fn fields<'a>(&'a self, to: &'a [u8]) -> Vec<Rlp<'a>> {
        vec![
            Rlp::Uint(self.nonce as u128),
            Rlp::Uint(self.gas_price),
            Rlp::Uint(self.gas as u128),
            Rlp::Bytes(to),
            Rlp::Uint(self.value),
            Rlp::Bytes(&self.data),
        ]
    }

    /// EIP-155 signing hash.
    pub fn signing_hash(&self) -> [u8; 32] {
        let to = self.to.map(|a| a.0.to_vec()).unwrap_or_default();
        let mut f = self.fields(&to);
        f.push(Rlp::Uint(self.chain_id as u128));
        f.push(Rlp::Uint(0));
        f.push(Rlp::Uint(0));
        keccak256(&rlp_encode(&Rlp::List(f)))
    }

    /// Signed raw transaction bytes (for eth_sendRawTransaction).
    pub fn sign(&self, sk: &SecretKey) -> Vec<u8> {
        let h = self.signing_hash();
        let sig = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(h), sk);
        let (rid, rs) = sig.serialize_compact();
        let v = i32::from(rid) as u128 + self.chain_id as u128 * 2 + 35;
        let to = self.to.map(|a| a.0.to_vec()).unwrap_or_default();
        let mut f = self.fields(&to);
        f.push(Rlp::Uint(v));
        f.push(Rlp::Bytes(trim_be(&rs[..32])));
        f.push(Rlp::Bytes(trim_be(&rs[32..])));
        rlp_encode(&Rlp::List(f))
    }
}

/// Signs and submits transactions from one key.
pub struct TxSender<'a> {
    pub rpc: &'a EthRpc,
    pub key: SecretKey,
    pub chain_id: u64,
}

impl<'a> TxSender<'a> {
    pub async fn new(rpc: &'a EthRpc, key: SecretKey) -> Result<Self> {
        let chain_id = rpc.chain_id().await?;
        Ok(Self { rpc, key, chain_id })
    }

    pub fn address(&self) -> Address {
        secret_address(&self.key)
    }

    /// Sign + broadcast; returns the tx hash. Gas is estimated (+25%).
    pub async fn send(&self, to: Option<&Address>, data: Vec<u8>) -> Result<Hash32> {
        let from = self.address();
        let gas = self.rpc.estimate_gas(&from, to, &data).await?;
        let tx = LegacyTx {
            nonce: self.rpc.get_transaction_count(&from).await?,
            gas_price: self.rpc.gas_price().await?.max(1),
            gas: gas + gas / 4 + 10_000,
            to: to.copied(),
            value: 0,
            data,
            chain_id: self.chain_id,
        };
        self.rpc.send_raw_transaction(&tx.sign(&self.key)).await
    }

    /// Send and wait for a successful receipt.
    pub async fn send_and_wait(&self, to: Option<&Address>, data: Vec<u8>) -> Result<Receipt> {
        let h = self.send(to, data).await?;
        let r = self.rpc.wait_receipt(&h, Duration::from_secs(30)).await?;
        if !r.status {
            bail!("transaction {h} reverted");
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eip712::parse_secret_key;

    #[test]
    fn rlp_basics() {
        assert_eq!(rlp_encode(&Rlp::Uint(0)), vec![0x80]);
        assert_eq!(rlp_encode(&Rlp::Uint(15)), vec![0x0f]);
        assert_eq!(rlp_encode(&Rlp::Uint(1024)), vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp_encode(&Rlp::Bytes(b"dog")), vec![0x83, b'd', b'o', b'g']);
        assert_eq!(rlp_encode(&Rlp::List(vec![])), vec![0xc0]);
        let lorem = b"Lorem ipsum dolor sit amet, consectetur adipisicing elit";
        let e = rlp_encode(&Rlp::Bytes(lorem));
        assert_eq!(&e[..2], &[0xb8, 0x38]);
        assert_eq!(
            rlp_encode(&Rlp::List(vec![Rlp::Bytes(b"cat"), Rlp::Bytes(b"dog")])),
            vec![0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']
        );
    }

    /// EIP-155 example transaction from the EIP text.
    #[test]
    fn eip155_example() {
        let tx = LegacyTx {
            nonce: 9,
            gas_price: 20_000_000_000,
            gas: 21000,
            to: Some(Address::from_hex("0x3535353535353535353535353535353535353535").unwrap()),
            value: 1_000_000_000_000_000_000,
            data: vec![],
            chain_id: 1,
        };
        assert_eq!(
            hex::encode(tx.signing_hash()),
            "daf5a779ae972f972197303d7b574746c7ef83eadac0f2791ad23db92e4c8e53"
        );
        let sk = parse_secret_key("4646464646464646464646464646464646464646464646464646464646464646").unwrap();
        assert_eq!(
            hex::encode(tx.sign(&sk)),
            "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a0\
             28ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83"
        );
    }
}
