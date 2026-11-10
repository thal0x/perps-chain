//! Fake signature scheme for tests: `action_bytes = borsh(Action)`, the
//! signer address sits in `signature.r[..20]`, `v == 27` means valid.

use borsh::BorshDeserialize;
use interfaces::*;

#[derive(Clone, Default)]
pub struct FakeVerifier;

pub fn fake_sig(signer: Address) -> Sig65 {
    let mut r = [0u8; 32];
    r[..20].copy_from_slice(&signer.0);
    Sig65 { r, s: [0u8; 32], v: 27 }
}

fn signer_of(sig: &Sig65) -> Result<Address, String> {
    if sig.v != 27 {
        return Err("bad signature".into());
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(&sig.r[..20]);
    Ok(Address(a))
}

impl TxVerifier for FakeVerifier {
    fn decode(&self, tx: &UserTx) -> Result<Action, String> {
        Action::try_from_slice(&tx.action_bytes).map_err(|e| e.to_string())
    }
    fn recover_signer(&self, tx: &UserTx, _action: &Action) -> Result<Address, String> {
        signer_of(&tx.signature)
    }
    fn recover_validator(&self, tx: &ValidatorTx, _chain_id: u64) -> Result<Address, String> {
        signer_of(&tx.signature)
    }
}

/// Build a fake-signed user tx.
pub fn user_tx(signer: Address, action: &Action, nonce: u64) -> Tx {
    Tx::User(UserTx {
        action_bytes: borsh::to_vec(action).expect("encode"),
        nonce,
        signature: fake_sig(signer),
        vault_address: None,
        expires_after: None,
    })
}

/// Build a fake-signed validator tx.
pub fn validator_tx(validator: Address, nonce: u64, action: ValidatorAction) -> Tx {
    Tx::Validator(ValidatorTx { validator, nonce, action, signature: fake_sig(validator) })
}
