use clap::{Parser, Subcommand};
use sim::gen::GenConfig;
use sim::genesis::{markets_by_name, GenesisOpts};
use sim::replay::{InvariantEvery, ReplayOpts};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sim", about = "hlbft simulator: block-log generation, offline replay, live load")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a block log with an embedded chain giving agents feedback.
    Gen {
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 200_000)]
        actions: u64,
        #[arg(long, default_value_t = 1.0)]
        sim_days: f64,
        #[arg(long, default_value_t = 6000)]
        block_ms: u64,
        #[arg(long, default_value_t = 4)]
        validators: usize,
        #[arg(long, default_value_t = 1000)]
        accounts: usize,
        /// Comma-separated market names (default: all built-in markets).
        #[arg(long)]
        markets: Option<String>,
        /// Fraction of intentionally invalid user txs, parts per million.
        #[arg(long, default_value_t = 10_000)]
        invalid_ppm: u32,
        #[arg(long, default_value_t = 1)]
        skewed_validators: usize,
        /// `state` (real HlChain) or `null` (no state; generator benchmark).
        #[arg(long, default_value = "state")]
        app: String,
        /// Write gen-time roots (`height root`).
        #[arg(long)]
        roots: Option<PathBuf>,
        /// Also write genesis.json + keys.json into this directory.
        #[arg(long)]
        genesis_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 1)]
        threads: usize,
        #[arg(short = 'o', long)]
        out: PathBuf,
    },
    /// Replay a log offline, checking invariants and printing per-block roots.
    Replay {
        log: PathBuf,
        /// every | never | N (every N blocks)
        #[arg(long, default_value = "every")]
        check_invariants: String,
        #[arg(long)]
        roots: Option<PathBuf>,
        #[arg(long, default_value = "state")]
        app: String,
        #[arg(long, default_value_t = 2)]
        threads: usize,
    },
    /// Write genesis.json + keys.json for a localnet.
    Genesis {
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 4)]
        validators: usize,
        #[arg(long, default_value_t = 1000)]
        accounts: usize,
        #[arg(long)]
        markets: Option<String>,
        #[arg(long, default_value_t = 9000)]
        p2p_base_port: u16,
        #[arg(long)]
        genesis_time_ms: Option<u64>,
        #[arg(short = 'o', long)]
        out: PathBuf,
    },
    /// Live load against node APIs over HTTP /exchange.
    Load(sim::load::LoadArgs),
    /// Print a log's header and footer.
    Info { log: PathBuf },
}

fn factory(name: &str) -> anyhow::Result<sim::AppFactory> {
    match name {
        "state" => Ok(sim::state_factory()),
        "null" => Ok(sim::null_factory()),
        o => anyhow::bail!("unknown --app {o}"),
    }
}

fn threads(n: usize) {
    let _ = rayon::ThreadPoolBuilder::new().num_threads(n.max(1)).build_global();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Gen {
            seed,
            actions,
            sim_days,
            block_ms,
            validators,
            accounts,
            markets,
            invalid_ppm,
            skewed_validators,
            app,
            roots,
            genesis_dir,
            threads: th,
            out,
        } => {
            threads(th);
            let cfg = GenConfig {
                seed,
                actions,
                sim_ms: (sim_days * 86_400_000.0).round() as u64,
                block_ms,
                validators,
                accounts,
                invalid_ppm,
                skewed_validators,
                markets: match markets {
                    Some(m) => markets_by_name(&m)?,
                    None => sim::default_markets(),
                },
                genesis_opts: GenesisOpts::default(),
                progress_ms: 5000,
            };
            if let Some(d) = genesis_dir {
                let (g, k) = sim::gen::keys_for(&cfg);
                sim::genesis::write_genesis_files(&d, &g, &k)?;
            }
            let f = factory(&app)?;
            let file = std::fs::File::create(&out)?;
            let mut rw = match roots {
                Some(p) => Some(BufWriter::new(std::fs::File::create(p)?)),
                None => None,
            };
            let mut werr = None;
            let s = sim::gen::generate(&cfg, &f, file, |h, r| {
                if let Some(w) = rw.as_mut() {
                    if let Err(e) = writeln!(w, "{h} {}", r.to_hex()) {
                        werr = Some(e);
                    }
                }
            })?;
            if let Some(e) = werr {
                return Err(e.into());
            }
            if let Some(mut w) = rw {
                w.flush()?;
            }
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        Cmd::Replay { log, check_invariants, roots, app, threads: th } => {
            threads(th);
            let inv: InvariantEvery = check_invariants.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let f = factory(&app)?;
            let mut rw = match roots {
                Some(p) => Some(BufWriter::new(std::fs::File::create(p)?)),
                None => None,
            };
            let opts = ReplayOpts { invariants: inv, progress_ms: 5000, max_violations: 1000 };
            let s = sim::replay::replay(std::fs::File::open(&log)?, &f, &opts, rw.as_mut().map(|w| w as &mut dyn Write))?;
            if let Some(mut w) = rw {
                w.flush()?;
            }
            println!("{}", serde_json::to_string_pretty(&s)?);
            if !s.violations.is_empty() {
                std::process::exit(2);
            }
        }
        Cmd::Genesis { seed, validators, accounts, markets, p2p_base_port, genesis_time_ms, out } => {
            let mut o = GenesisOpts { p2p_base_port, ..Default::default() };
            if let Some(t) = genesis_time_ms {
                o.genesis_time_ms = t;
            }
            let m = match markets {
                Some(m) => markets_by_name(&m)?,
                None => sim::default_markets(),
            };
            let (g, k) = sim::genesis_with(seed, validators, accounts, &m, &o);
            sim::genesis::write_genesis_files(&out, &g, &k)?;
            println!("wrote {}/genesis.json and keys.json (genesis hash {})", out.display(), sim::genesis::genesis_hash(&g));
        }
        Cmd::Load(a) => sim::load::run(a)?,
        Cmd::Info { log } => {
            let mut r = sim::log::LogReader::open(&log)?;
            let h = r.header().clone();
            println!("genesis_hash {}\nseed {}\nparams {}", h.genesis_hash, h.seed, serde_json::to_string(&h.params)?);
            while r.next_block()?.is_some() {}
            println!("footer {}", serde_json::to_string(r.footer().unwrap())?);
        }
    }
    Ok(())
}
