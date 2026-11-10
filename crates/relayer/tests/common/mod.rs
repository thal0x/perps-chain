//! Test helpers: spawn Anvil on a random port, build + deploy the Foundry
//! contracts, and a fake `BridgeHost`.
#![allow(dead_code)]

use interfaces::*;
use parking_lot::Mutex;
use relayer::abi::{self, Token};
use relayer::contract::{approve_calldata, deposit_calldata, mint_calldata};
use relayer::eip712::{parse_secret_key, secret_address, sign_digest};
use relayer::rpc::EthRpc;
use relayer::txsign::TxSender;
use secp256k1::SecretKey;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

pub fn have_tool(name: &str) -> bool {
    Command::new(name).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

/// Returns false (and prints why) when anvil/forge are unavailable.
pub fn tools_available() -> bool {
    for t in ["anvil", "forge"] {
        if !have_tool(t) {
            eprintln!("SKIPPED: `{t}` not on PATH; install Foundry to run the Anvil integration tests");
            return false;
        }
    }
    true
}

pub struct Anvil {
    pub port: u16,
    child: Option<Child>,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl Anvil {
    pub async fn start() -> Self {
        let mut a = Anvil { port: free_port(), child: None };
        a.spawn().await;
        a
    }

    async fn spawn(&mut self) {
        for attempt in 0..3 {
            let child = Command::new("anvil")
                .args(["--port", &self.port.to_string(), "--silent"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn anvil");
            self.child = Some(child);
            let rpc = self.rpc();
            for _ in 0..100 {
                if rpc.chain_id().await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            self.kill();
            if attempt < 2 {
                self.port = free_port();
            }
        }
        panic!("anvil did not start");
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn rpc(&self) -> EthRpc {
        EthRpc::new(self.url())
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Kill and restart on the same port (all chain state is lost).
    pub async fn restart(&mut self) {
        self.kill();
        let port = self.port;
        // wait for the port to be released
        for _ in 0..100 {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let rpc = self.rpc();
        let child = Command::new("anvil")
            .args(["--port", &port.to_string(), "--silent"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn anvil");
        self.child = Some(child);
        for _ in 0..100 {
            if rpc.chain_id().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("anvil did not restart");
    }
}

impl Drop for Anvil {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct Artifacts {
    pub usdc: Vec<u8>,
    pub bridge: Vec<u8>,
}

/// `forge build` the bridge into a private out dir (once per test binary).
pub fn artifacts() -> &'static Artifacts {
    static A: OnceLock<Artifacts> = OnceLock::new();
    A.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bridge");
        let tmp = std::env::temp_dir().join(format!("hlbft-relayer-forge-{}", std::process::id()));
        let out = tmp.join("out");
        let cache = tmp.join("cache");
        let st = Command::new("forge")
            .current_dir(&root)
            .args(["build", "--out", out.to_str().unwrap(), "--cache-path", cache.to_str().unwrap()])
            .stdout(Stdio::null())
            .status()
            .expect("run forge build");
        assert!(st.success(), "forge build failed");
        let read = |name: &str| -> Vec<u8> {
            let p = out.join(format!("{name}.sol/{name}.json"));
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            let h = v["bytecode"]["object"].as_str().unwrap();
            hex::decode(h.trim_start_matches("0x")).unwrap()
        };
        let a = Artifacts { usdc: read("MockUSDC"), bridge: read("Bridge") };
        let _ = std::fs::remove_dir_all(&tmp);
        a
    })
}

pub fn dev_key(i: usize) -> SecretKey {
    parse_secret_key(relayer::ANVIL_DEV_KEYS[i]).unwrap()
}

/// Deterministic validator keys, sorted by address.
pub fn validator_keys(n: usize, seed: u8) -> Vec<SecretKey> {
    let mut ks: Vec<SecretKey> =
        (0..n).map(|i| SecretKey::from_slice(&abi::keccak256(&[seed, i as u8])).unwrap()).collect();
    ks.sort_by_key(secret_address);
    ks
}

pub struct Deployment {
    pub usdc: Address,
    pub bridge: Address,
    pub chain_id: u64,
}

impl Deployment {
    pub fn params(&self) -> BridgeParams {
        BridgeParams { eth_chain_id: self.chain_id, contract: self.bridge, min_deposit: Usd::from_raw(5_000_000) }
    }
}

/// Deploy MockUSDC + Bridge from dev account 0 and fund the bridge with 1M USDC.
pub async fn deploy(rpc: &EthRpc, validators: &[Address], powers: &[u128], epoch: u64) -> Deployment {
    let art = artifacts();
    let tx = TxSender::new(rpc, dev_key(0)).await.unwrap();
    let r = tx.send_and_wait(None, art.usdc.clone()).await.unwrap();
    let usdc = r.contract_address.unwrap();
    let mut code = art.bridge.clone();
    code.extend(abi::encode(&[
        Token::address(&usdc),
        Token::u64(5_000_000),
        Token::Array(validators.iter().map(Token::address).collect()),
        Token::Array(powers.iter().map(|p| Token::u128(*p)).collect()),
        Token::u64(epoch),
    ]));
    let r = tx.send_and_wait(None, code).await.unwrap();
    let bridge = r.contract_address.unwrap();
    tx.send_and_wait(Some(&usdc), mint_calldata(&bridge, 1_000_000_000_000)).await.unwrap();
    Deployment { usdc, bridge, chain_id: tx.chain_id }
}

/// Mint to dev account `i`, approve and deposit `amount` (base units). Returns tx hash.
pub async fn user_deposit(rpc: &EthRpc, d: &Deployment, i: usize, amount: u128) -> Hash32 {
    let minter = TxSender::new(rpc, dev_key(0)).await.unwrap();
    let user = TxSender::new(rpc, dev_key(i)).await.unwrap();
    minter.send_and_wait(Some(&d.usdc), mint_calldata(&user.address(), amount)).await.unwrap();
    user.send_and_wait(Some(&d.usdc), approve_calldata(&d.bridge, amount)).await.unwrap();
    user.send_and_wait(Some(&d.bridge), deposit_calldata(amount)).await.unwrap().tx_hash
}

pub fn genesis_with(bridge: BridgeParams) -> Genesis {
    Genesis {
        chain_id: 1337,
        is_mainnet: false,
        genesis_time_ms: 0,
        validators: ValidatorSet { validators: vec![] },
        markets: vec![],
        balances: vec![],
        spot_tokens: vec![],
        spot_balances: vec![],
        params: ChainParams::default(),
        bridge,
    }
}

/// In-memory `BridgeHost`: records submitted actions.
pub struct FakeHost {
    pub key: SecretKey,
    pub is_validator: bool,
    pub genesis: Genesis,
    pub actions: Mutex<Vec<ValidatorAction>>,
    pub processed: Mutex<BTreeSet<u64>>,
    pub pending: Mutex<Vec<PendingWithdrawal>>,
}

impl FakeHost {
    pub fn new(key: SecretKey, bridge: BridgeParams) -> Self {
        Self {
            key,
            is_validator: true,
            genesis: genesis_with(bridge),
            actions: Mutex::new(vec![]),
            processed: Mutex::new(BTreeSet::new()),
            pending: Mutex::new(vec![]),
        }
    }
    pub fn address(&self) -> Address {
        secret_address(&self.key)
    }
    pub fn take_actions(&self) -> Vec<ValidatorAction> {
        std::mem::take(&mut *self.actions.lock())
    }
}

impl BridgeHost for FakeHost {
    fn validator(&self) -> Option<Address> {
        self.is_validator.then(|| self.address())
    }
    fn submit_validator_action(&self, a: ValidatorAction) -> Result<(), String> {
        self.actions.lock().push(a);
        Ok(())
    }
    fn pending_withdrawals(&self) -> Vec<PendingWithdrawal> {
        self.pending.lock().clone()
    }
    fn processed_deposit(&self, id: u64) -> bool {
        self.processed.lock().contains(&id)
    }
    fn sign_digest(&self, digest: &[u8; 32]) -> Option<Sig65> {
        Some(sign_digest(&self.key, digest))
    }
    fn genesis(&self) -> Genesis {
        self.genesis.clone()
    }
}
