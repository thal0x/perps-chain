//! Primitive identifiers shared by every crate.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Block height (first block after genesis = 1; genesis = 0).
pub type Height = u64;
/// Consensus view / round number.
pub type View = u64;
/// Milliseconds since unix epoch. Block time is chosen by the proposer and
/// validated by voters; the state machine never reads the wall clock.
pub type TimestampMs = u64;
/// Perp asset index (HL "asset": index into `meta.universe`).
pub type AssetId = u32;
/// Globally unique, monotonically assigned order id.
pub type Oid = u64;
/// Globally unique, monotonically assigned trade id.
pub type Tid = u64;
/// Client order id (HL "cloid", 16 bytes, hex on the wire).
pub type Cloid = [u8; 16];

macro_rules! hex_bytes_type {
    ($name:ident, $n:expr) => {
        #[derive(
            Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, BorshSerialize, BorshDeserialize,
        )]
        pub struct $name(pub [u8; $n]);

        impl $name {
            pub const ZERO: Self = Self([0u8; $n]);
            pub fn to_hex(&self) -> String {
                format!("0x{}", hex::encode(self.0))
            }
            pub fn from_hex(s: &str) -> Result<Self, crate::error::ParseError> {
                let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                let v = hex::decode(s).map_err(|_| crate::error::ParseError::BadHex)?;
                if v.len() != $n {
                    return Err(crate::error::ParseError::BadLength { expected: $n, got: v.len() });
                }
                let mut out = [0u8; $n];
                out.copy_from_slice(&v);
                Ok(Self(out))
            }
            pub fn as_bytes(&self) -> &[u8; $n] {
                &self.0
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.to_hex())
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.to_hex())
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_hex())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <String as Deserialize>::deserialize(d)?;
                Self::from_hex(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

hex_bytes_type!(Address, 20);
hex_bytes_type!(Hash32, 32);

/// Secp256k1 recoverable signature in Ethereum form. `v` is 27/28 on the HL
/// wire; implementations must accept 0/1 as well and normalise to 27/28.
#[derive(Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Default)]
pub struct Sig65 {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u8,
}

impl fmt::Debug for Sig65 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sig65(r=0x{},s=0x{},v={})", hex::encode(self.r), hex::encode(self.s), self.v)
    }
}

/// HL wire form: `{"r": "0x..", "s": "0x..", "v": 27}`.
impl Serialize for Sig65 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Sig65", 3)?;
        st.serialize_field("r", &format!("0x{}", hex::encode(self.r)))?;
        st.serialize_field("s", &format!("0x{}", hex::encode(self.s)))?;
        st.serialize_field("v", &self.v)?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for Sig65 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            r: String,
            s: String,
            v: u8,
        }
        let w = <Wire as Deserialize>::deserialize(d)?;
        // r and s may be sent without left zero padding by some signers.
        fn word(x: &str) -> Result<[u8; 32], String> {
            let x = x.strip_prefix("0x").unwrap_or(x);
            let x = if x.len() % 2 == 1 { format!("0{x}") } else { x.to_string() };
            let b = hex::decode(&x).map_err(|e| e.to_string())?;
            if b.len() > 32 {
                return Err("signature word longer than 32 bytes".into());
            }
            let mut out = [0u8; 32];
            out[32 - b.len()..].copy_from_slice(&b);
            Ok(out)
        }
        Ok(Sig65 {
            r: word(&w.r).map_err(serde::de::Error::custom)?,
            s: word(&w.s).map_err(serde::de::Error::custom)?,
            v: w.v,
        })
    }
}

/// Hash of a transaction (blake3 of its borsh encoding).
pub type TxHash = Hash32;
/// Block id (blake3 of the borsh-encoded header).
pub type BlockId = Hash32;

/// blake3 helper used for every non-Ethereum hash in the system.
pub fn blake3_hash(bytes: &[u8]) -> Hash32 {
    Hash32(*blake3::hash(bytes).as_bytes())
}

/// blake3 of the borsh encoding of `v`.
pub fn hash_borsh<T: BorshSerialize>(v: &T) -> Hash32 {
    let mut h = blake3::Hasher::new();
    let mut w = HashWriter(&mut h);
    v.serialize(&mut w).expect("hash writer is infallible");
    Hash32(*h.finalize().as_bytes())
}

struct HashWriter<'a>(&'a mut blake3::Hasher);
impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
