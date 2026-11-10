use anyhow::Result;
use clap::{Parser, Subcommand};
use node::config::{Resolved, RunArgs};
use node::testnet::{self, GenOpts};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "hlnode", about = "hlbft validator node")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a local testnet (genesis, keys, per-node configs).
    TestnetGen {
        #[arg(long)]
        validators: usize,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 26600)]
        base_p2p: u16,
        #[arg(long, default_value_t = 3001)]
        base_api: u16,
        #[arg(long, default_value_t = 0)]
        accounts: usize,
        #[arg(long)]
        fund_sdk_examples: bool,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Default: now (rounded down to the second).
        #[arg(long)]
        genesis_time_ms: Option<u64>,
        #[arg(long)]
        bridge_contract: Option<String>,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Benchmark profile: API rate limits off, invariants off.
        #[arg(long)]
        bench: bool,
    },
    /// Run a validator.
    Run(RunArgs),
}

fn main() {
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::TestnetGen {
            validators,
            out,
            base_p2p,
            base_api,
            accounts,
            fund_sdk_examples,
            seed,
            genesis_time_ms,
            bridge_contract,
            host,
            bench,
        } => {
            let o = GenOpts {
                validators,
                base_p2p,
                base_api,
                accounts,
                fund_sdk_examples,
                seed,
                genesis_time_ms: genesis_time_ms.unwrap_or_else(|| node::node::now_ms() / 1000 * 1000),
                bridge_contract,
                host,
                bench,
            };
            testnet::write(&out, &o).map(|g| {
                println!(
                    "{}",
                    serde_json::json!({"out": out, "validators": g.validators.len(), "accounts": g.accounts.len(),
                        "chain_id": g.genesis.chain_id})
                );
            })
        }
        Cmd::Run(a) => run(a),
    };
    if let Err(e) = r {
        eprintln!("hlnode: fatal: {e:#}");
        tracing::error!("fatal: {e:#}");
        std::process::exit(1);
    }
}

fn init_logging(cfg: &Resolved) -> Result<()> {
    let f = std::fs::OpenOptions::new().create(true).append(true).open(cfg.home.join("node.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(f)).init();
    Ok(())
}

fn run(a: RunArgs) -> Result<()> {
    std::fs::create_dir_all(&a.home)?;
    let cfg = Resolved::from_args(&a)?;
    init_logging(&cfg)?;
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, term.clone())?;
    let n = node::Node::start(cfg)?;
    while !term.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    n.shutdown();
    tracing::info!("clean shutdown");
    Ok(())
}
