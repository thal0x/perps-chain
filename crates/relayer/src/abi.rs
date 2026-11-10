//! Minimal hand-rolled Solidity ABI encoding/decoding and keccak helpers.
//!
//! Only what the bridge needs: static words (uint/address/bytes32), dynamic
//! arrays and (static or dynamic) tuples, following the standard head/tail
//! layout of the Solidity ABI spec.

use interfaces::{Address, Hash32};
use sha3::{Digest, Keccak256};

pub type Word = [u8; 32];

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

/// 4-byte function selector of a canonical signature, e.g. `"deposit(uint256)"`.
pub fn selector(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

/// Event topic0 of a canonical event signature.
pub fn event_topic(sig: &str) -> Hash32 {
    Hash32(keccak256(sig.as_bytes()))
}

pub fn word_u64(v: u64) -> Word {
    word_u128(v as u128)
}

pub fn word_u128(v: u128) -> Word {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

pub fn word_address(a: &Address) -> Word {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&a.0);
    w
}

/// Decode a word as an address; errors if the upper 12 bytes are non-zero.
pub fn address_from_word(w: &Word) -> anyhow::Result<Address> {
    anyhow::ensure!(w[..12].iter().all(|b| *b == 0), "dirty address word");
    let mut a = [0u8; 20];
    a.copy_from_slice(&w[12..]);
    Ok(Address(a))
}

/// Decode a word as a u128; errors on overflow.
pub fn u128_from_word(w: &Word) -> anyhow::Result<u128> {
    anyhow::ensure!(w[..16].iter().all(|b| *b == 0), "uint256 does not fit in u128");
    Ok(u128::from_be_bytes(w[16..].try_into().unwrap()))
}

pub fn u64_from_word(w: &Word) -> anyhow::Result<u64> {
    let v = u128_from_word(w)?;
    u64::try_from(v).map_err(|_| anyhow::anyhow!("uint256 does not fit in u64"))
}

pub fn bool_from_word(w: &Word) -> anyhow::Result<bool> {
    anyhow::ensure!(w[..31].iter().all(|b| *b == 0) && w[31] <= 1, "bad bool word");
    Ok(w[31] == 1)
}

/// An ABI value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// Any static 32-byte value (uintN, address, bytes32, bool, ...), already encoded.
    Word(Word),
    /// `T[]` (dynamic-length array).
    Array(Vec<Token>),
    /// `(T1, T2, ...)`.
    Tuple(Vec<Token>),
}

impl Token {
    pub fn u64(v: u64) -> Self {
        Token::Word(word_u64(v))
    }
    pub fn u128(v: u128) -> Self {
        Token::Word(word_u128(v))
    }
    pub fn address(a: &Address) -> Self {
        Token::Word(word_address(a))
    }
    pub fn bytes32(b: [u8; 32]) -> Self {
        Token::Word(b)
    }

    fn is_dynamic(&self) -> bool {
        match self {
            Token::Word(_) => false,
            Token::Array(_) => true,
            Token::Tuple(ts) => ts.iter().any(Token::is_dynamic),
        }
    }

    /// Size of this token's head slot in bytes.
    fn head_len(&self) -> usize {
        match self {
            Token::Tuple(ts) if !self.is_dynamic() => ts.iter().map(Token::head_len).sum(),
            Token::Word(_) => 32,
            _ => 32, // offset
        }
    }

    /// Encoding of the token itself (for dynamic tokens: what goes in the tail).
    fn encode_inner(&self, out: &mut Vec<u8>) {
        match self {
            Token::Word(w) => out.extend_from_slice(w),
            Token::Tuple(ts) => encode_sequence(ts, out),
            Token::Array(ts) => {
                out.extend_from_slice(&word_u64(ts.len() as u64));
                encode_sequence(ts, out);
            }
        }
    }
}

/// Encode a sequence of tokens as `(T1, ..., Tn)` using head/tail layout.
fn encode_sequence(ts: &[Token], out: &mut Vec<u8>) {
    let heads_len: usize = ts.iter().map(Token::head_len).sum();
    let mut tail = Vec::new();
    for t in ts {
        if t.is_dynamic() {
            out.extend_from_slice(&word_u64((heads_len + tail.len()) as u64));
            t.encode_inner(&mut tail);
        } else {
            t.encode_inner(out);
        }
    }
    out.extend_from_slice(&tail);
}

/// ABI-encode function arguments (no selector), i.e. `abi.encode(args...)`.
pub fn encode(args: &[Token]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_sequence(args, &mut out);
    out
}

/// Selector + encoded arguments.
pub fn encode_call(sig: &str, args: &[Token]) -> Vec<u8> {
    let mut out = selector(sig).to_vec();
    out.extend_from_slice(&encode(args));
    out
}

/// Read the `i`-th 32-byte word of `data`.
pub fn word_at(data: &[u8], i: usize) -> anyhow::Result<Word> {
    let start = i.checked_mul(32).ok_or_else(|| anyhow::anyhow!("offset overflow"))?;
    let end = start + 32;
    anyhow::ensure!(data.len() >= end, "abi data too short ({} < {end})", data.len());
    Ok(data[start..end].try_into().unwrap())
}

/// Decode a dynamic array of static words whose offset is stored in head word `head_idx`.
pub fn decode_word_array(data: &[u8], head_idx: usize) -> anyhow::Result<Vec<Word>> {
    let off = u64_from_word(&word_at(data, head_idx)?)? as usize;
    anyhow::ensure!(off % 32 == 0, "unaligned array offset");
    let base = off / 32;
    let len = u64_from_word(&word_at(data, base)?)? as usize;
    anyhow::ensure!(len <= data.len() / 32, "array length too large");
    (0..len).map(|i| word_at(data, base + 1 + i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(h: &str) -> Address {
        Address::from_hex(h).unwrap()
    }

    #[test]
    fn selectors_match_solidity() {
        // Values from `cast sig`.
        assert_eq!(hex::encode(selector("transfer(address,uint256)")), "a9059cbb");
        assert_eq!(hex::encode(selector("approve(address,uint256)")), "095ea7b3");
        assert_eq!(hex::encode(selector("deposit(uint256)")), "b6b55f25");
        assert_eq!(
            event_topic("Transfer(address,address,uint256)").to_hex(),
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
    }

    #[test]
    fn static_args() {
        let a = addr("0x70997970C51812dc3A010C7d01b50e0d17dc79C8");
        let data = encode_call("transfer(address,uint256)", &[Token::address(&a), Token::u64(1_000_000)]);
        assert_eq!(
            hex::encode(data),
            "a9059cbb00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c8\
             00000000000000000000000000000000000000000000000000000000000f4240"
        );
    }

    #[test]
    fn withdraw_calldata_matches_cast() {
        // cast calldata "withdraw((uint64,address,uint256),(uint8,bytes32,bytes32)[])" \
        //   "(7,0x70997970C51812dc3A010C7d01b50e0d17dc79C8,499000000)" \
        //   "[(27,0x11..11,0x22..22),(28,0x33..33,0x44..44)]"
        let w = Token::Tuple(vec![
            Token::u64(7),
            Token::address(&addr("0x70997970C51812dc3A010C7d01b50e0d17dc79C8")),
            Token::u64(499_000_000),
        ]);
        let sigs = Token::Array(vec![
            Token::Tuple(vec![Token::u64(27), Token::bytes32([0x11; 32]), Token::bytes32([0x22; 32])]),
            Token::Tuple(vec![Token::u64(28), Token::bytes32([0x33; 32]), Token::bytes32([0x44; 32])]),
        ]);
        let data = encode_call("withdraw((uint64,address,uint256),(uint8,bytes32,bytes32)[])", &[w, sigs]);
        assert_eq!(hex::encode(data), WITHDRAW_CAST);
    }

    #[test]
    fn update_validator_set_calldata_matches_cast() {
        // cast calldata "updateValidatorSet(address[],uint256[],uint64,(uint8,bytes32,bytes32)[])" \
        //   "[0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65,0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC]" \
        //   "[1,2]" 5 "[(27,0x11..11,0x22..22)]"
        let data = encode_call(
            "updateValidatorSet(address[],uint256[],uint64,(uint8,bytes32,bytes32)[])",
            &[
                Token::Array(vec![
                    Token::address(&addr("0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65")),
                    Token::address(&addr("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC")),
                ]),
                Token::Array(vec![Token::u64(1), Token::u64(2)]),
                Token::u64(5),
                Token::Array(vec![Token::Tuple(vec![
                    Token::u64(27),
                    Token::bytes32([0x11; 32]),
                    Token::bytes32([0x22; 32]),
                ])]),
            ],
        );
        assert_eq!(hex::encode(data), UPDATE_CAST);
    }

    #[test]
    fn decode_arrays_roundtrip() {
        let a1 = addr("0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65");
        let a2 = addr("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
        let data = encode(&[
            Token::Array(vec![Token::address(&a1), Token::address(&a2)]),
            Token::Array(vec![Token::u64(3), Token::u64(4)]),
        ]);
        let vs = decode_word_array(&data, 0).unwrap();
        let ps = decode_word_array(&data, 1).unwrap();
        assert_eq!(address_from_word(&vs[0]).unwrap(), a1);
        assert_eq!(address_from_word(&vs[1]).unwrap(), a2);
        assert_eq!(u64_from_word(&ps[1]).unwrap(), 4);
        assert!(decode_word_array(&data[..64], 0).is_err());
        assert!(address_from_word(&[0xff; 32]).is_err());
        assert!(u64_from_word(&word_u128(u64::MAX as u128 + 1)).is_err());
        assert!(bool_from_word(&word_u64(1)).unwrap());
        assert!(bool_from_word(&word_u64(2)).is_err());
    }

    #[test]
    fn empty_array() {
        let data = encode(&[Token::Array(vec![])]);
        assert_eq!(data.len(), 64);
        assert_eq!(decode_word_array(&data, 0).unwrap().len(), 0);
    }

    const WITHDRAW_CAST: &str = include_str!("../testdata/withdraw_calldata.hex");
    const UPDATE_CAST: &str = include_str!("../testdata/update_calldata.hex");
}
