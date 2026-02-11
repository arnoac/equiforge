use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use equiforge::core::chain::Chain;
use equiforge::core::params::*;
use equiforge::miner::{self, MinerConfig};
use equiforge::network::{self, NodeState};
use equiforge::rpc;
use equiforge::wallet::{self, Wallet};

const DEFAULT_DATA_DIR: &str = "equiforge_data";
const DEFAULT_P2P_PORT: u16 = 9333;

#[derive(Parser)]
#[command(name = "equiforge", version = "1.0.0")]
#[command(about = "EquiForge - A fair, accessible blockchain network")]
struct Cli {
    #[arg(long, default_value = DEFAULT_DATA_DIR, global = true)]
    data_dir: String,
    #[arg(long, default_value_t = DEFAULT_P2P_PORT, global = true)]
    port: u16,
    /// Wallet password (for encrypted wallets)
    #[arg(long, global = true)]
    password: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new node
    Init,
    /// Run a full node
    Node {
        #[arg(short, long)]
        connect: Vec<String>,
        #[arg(short, long)]
        mine: bool,
        #[arg(short, long, default_value_t = 0)]
        threads: usize,
    },
    /// Send EQF to an address
    Send {
        #[arg(short, long)]
        to: String,
        #[arg(short, long)]
        amount: f64,
        #[arg(short, long, default_value_t = 0.0001)]
        fee: f64,
    },
    /// Show balance
    Balance { address: Option<String> },
    /// Wallet management
    Wallet {
        #[command(subcommand)]
        action: WalletAction,
    },
    /// Show blockchain info
    Info,
    /// Show connected peers
    Peers,
    /// Mine blocks for testing (in-memory)
    TestMine {
        #[arg(default_value_t = 5)]
        count: u64,
    },
}

#[derive(Subcommand)]
enum WalletAction {
    /// Show wallet addresses
    Show,
    /// Generate a new receiving address
    NewAddress,
    /// Encrypt the wallet with a password
    Encrypt {
        #[arg(short, long)]
        password: String,
    },
    /// Remove wallet encryption
    Decrypt {
        #[arg(short, long)]
        password: String,
    },
}

fn wallet_path(data_dir: &str) -> PathBuf { PathBuf::from(data_dir).join("wallet.json") }

fn load_wallet(data_dir: &str, password: Option<&str>) -> Wallet {
    Wallet::load_or_create_with_password(&wallet_path(data_dir), "node", password)
}

fn format_eqf(base_units: u64) -> String {
    let whole = base_units / COIN;
    let frac = base_units % COIN;
    if frac == 0 { format!("{}", whole) }
    else { format!("{}.{:08}", whole, frac).trim_end_matches('0').to_string() }
}

fn parse_eqf(amount: f64) -> u64 { (amount * COIN as f64).round() as u64 }
fn rpc_port(p2p: u16) -> u16 { p2p + rpc::RPC_PORT_OFFSET }

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("equiforge=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    let data_dir = &cli.data_dir;
    let port = cli.port;
    let pw = cli.password.as_deref();

    match cli.command {
        Commands::Init => {
            std::fs::create_dir_all(data_dir).unwrap();
            let chain = open_chain(data_dir);
            let wallet = load_wallet(data_dir, pw);
            println!("🔨 EquiForge initialized!");
            println!("  Data:    {}", data_dir);
            println!("  Height:  {}", chain.height);
            println!("  Genesis: {}", hex::encode(chain.tip));
            println!("  Wallet:  {}", wallet.primary_address());
            println!("  Encrypted: {}", wallet.is_encrypted());
            println!("\n  Run: equiforge node --mine");
        }

        Commands::Node { connect, mine, threads } => {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(run_node(data_dir, port, connect, mine, threads, pw));
        }

        Commands::Info => {
            if let Some(r) = rpc::try_rpc_call(rpc_port(port), "getinfo", serde_json::json!([])) {
                println!("📊 EquiForge (via node)");
                println!("  Height:     {}", r["height"]);
                println!("  Tip:        {}", r["tip"].as_str().unwrap_or("?"));
                println!("  Difficulty: {:.2}", r["fractional_difficulty"].as_f64().unwrap_or(0.0));
                println!("  UTXOs:      {}", r["utxos"]);
                println!("  Peers:      {}", r["peers"]);
                println!("  Mempool:    {}", r["mempool"]);
                println!("  Banned:     {}", r["banned"]);
                println!("  Reward:     {} EQF", r["block_reward"]);
            } else {
                let chain = open_chain(data_dir);
                println!("📊 EquiForge (from disk)");
                println!("  Height:     {}", chain.height);
                println!("  Tip:        {}", hex::encode(chain.tip));
                println!("  Difficulty: {:.2}", chain.fractional_difficulty());
                println!("  UTXOs:      {}", chain.utxo_set.len());
                println!("  Reward:     {} EQF", format_eqf(block_reward(chain.height)));
            }
        }

        Commands::Peers => {
            match rpc::rpc_call(rpc_port(port), "getpeerinfo", serde_json::json!([])) {
                Ok(peers) => {
                    if let Some(arr) = peers.as_array() {
                        if arr.is_empty() {
                            println!("No connected peers.");
                        } else {
                            println!("🌐 Connected peers ({}):", arr.len());
                            for p in arr {
                                println!("  {} v{} height={}",
                                    p["address"].as_str().unwrap_or("?"),
                                    p["version"],
                                    p["best_height"]);
                            }
                        }
                    }
                }
                Err(e) => eprintln!("❌ {}", e),
            }
        }

        Commands::Balance { address } => {
            match address {
                Some(addr) => {
                    if let Some(r) = rpc::try_rpc_call(rpc_port(port), "getbalance", serde_json::json!([addr])) {
                        println!("💰 {}: {} EQF", addr, r["balance"]);
                    } else {
                        let chain = open_chain(data_dir);
                        match wallet::address_to_pubkey_hash(&addr) {
                            Some(hash) => println!("💰 {}: {} EQF", addr, format_eqf(chain.utxo_set.balance_of(&hash))),
                            None => { eprintln!("❌ Invalid address"); std::process::exit(1); }
                        }
                    }
                }
                None => {
                    let wallet = load_wallet(data_dir, pw);
                    let use_rpc = rpc::try_rpc_call(rpc_port(port), "getinfo", serde_json::json!([])).is_some();
                    println!("💰 Wallet:");
                    let mut total: u64 = 0;
                    for (i, kp) in wallet.keypairs.iter().enumerate() {
                        let addr = kp.address();
                        let bal = if use_rpc {
                            rpc::try_rpc_call(rpc_port(port), "getbalance", serde_json::json!([addr]))
                                .and_then(|r| r["balance_base"].as_u64()).unwrap_or(0)
                        } else {
                            match Chain::open(data_dir) {
                                Ok(c) => c.utxo_set.balance_of(&kp.pubkey_hash()),
                                Err(_) => 0,
                            }
                        };
                        total += bal;
                        if bal > 0 || i == 0 {
                            println!("  {} {} EQF{}", addr, format_eqf(bal), if i == 0 { " (primary)" } else { "" });
                        }
                    }
                    println!("  Total: {} EQF", format_eqf(total));
                }
            }
        }

        Commands::Send { to, amount, fee } => {
            let wallet = load_wallet(data_dir, pw);
            let recipient_hash = match wallet::address_to_pubkey_hash(&to) {
                Some(h) => h,
                None => { eprintln!("❌ Invalid address: {}", to); std::process::exit(1); }
            };
            let amount_base = parse_eqf(amount);
            let fee_base = parse_eqf(fee);

            if let Some(info) = rpc::try_rpc_call(rpc_port(port), "getinfo", serde_json::json!([])) {
                let current_height = info["height"].as_u64().unwrap_or(0);
                let mut utxo_set = equiforge::core::chain::UtxoSet::new();
                for kp in &wallet.keypairs {
                    let addr = kp.address();
                    if let Some(utxos) = rpc::try_rpc_call(rpc_port(port), "listunspent", serde_json::json!([addr])) {
                        if let Some(arr) = utxos.as_array() {
                            for u in arr {
                                let txid_hex = u["txid"].as_str().unwrap_or("");
                                let vout = u["vout"].as_u64().unwrap_or(0) as u32;
                                let amt = u["amount_base"].as_u64().unwrap_or(0);
                                let h = u["height"].as_u64().unwrap_or(0);
                                let cb = u["coinbase"].as_bool().unwrap_or(false);
                                if let Ok(b) = hex::decode(txid_hex) {
                                    if b.len() == 32 {
                                        let mut txid = [0u8; 32]; txid.copy_from_slice(&b);
                                        utxo_set.add(
                                            OutPoint { txid, vout },
                                            equiforge::core::chain::UtxoEntry {
                                                output: TxOutput { amount: amt, pubkey_hash: kp.pubkey_hash() },
                                                height: h, is_coinbase: cb,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                let tx = match wallet.create_send_tx(&utxo_set, recipient_hash, amount_base, fee_base, current_height) {
                    Ok(tx) => tx,
                    Err(e) => { eprintln!("❌ {}", e); std::process::exit(1); }
                };
                println!("📤 Sending {} EQF to {} (fee: {} EQF)", format_eqf(amount_base), to, format_eqf(fee_base));
                let tx_json = serde_json::to_value(&tx).unwrap();
                match rpc::rpc_call(rpc_port(port), "sendrawtransaction", serde_json::json!([tx_json])) {
                    Ok(r) => println!("  ✅ TX: {}", r["txid"].as_str().unwrap_or("?")),
                    Err(e) => { eprintln!("  ❌ {}", e); std::process::exit(1); }
                }
            } else {
                let chain = open_chain(data_dir);
                let current_height = chain.height;
                let tx = match wallet.create_send_tx(&chain.utxo_set, recipient_hash, amount_base, fee_base, current_height) {
                    Ok(tx) => tx,
                    Err(e) => { eprintln!("❌ {}", e); std::process::exit(1); }
                };
                drop(chain);
                let path = PathBuf::from(data_dir).join("pending_tx.json");
                std::fs::write(&path, serde_json::to_string_pretty(&tx).unwrap()).unwrap();
                println!("📤 TX saved to {}. Start node to broadcast.", path.display());
            }
        }

        Commands::Wallet { action } => {
            match action {
                WalletAction::Show => {
                    let wallet = load_wallet(data_dir, pw);
                    println!("🔑 Wallet: {}", wallet_path(data_dir).display());
                    println!("  Encrypted: {}", wallet.is_encrypted());
                    println!("  Addresses: {}", wallet.keypairs.len());
                    for (i, kp) in wallet.keypairs.iter().enumerate() {
                        println!("  [{}] {}{}", i, kp.address(), if i == 0 { " (primary)" } else { "" });
                    }
                }
                WalletAction::NewAddress => {
                    let mut wallet = load_wallet(data_dir, pw);
                    let addr = wallet.new_address();
                    println!("🔑 New address: {}", addr);
                }
                WalletAction::Encrypt { password } => {
                    let mut wallet = load_wallet(data_dir, pw);
                    if wallet.is_encrypted() {
                        eprintln!("⚠️  Wallet is already encrypted. Decrypt first to change password.");
                        std::process::exit(1);
                    }
                    wallet.set_password(&password);
                    println!("🔒 Wallet encrypted. Use --password to access it.");
                }
                WalletAction::Decrypt { password } => {
                    let mut wallet = load_wallet(data_dir, Some(&password));
                    wallet.remove_password();
                    println!("🔓 Wallet decrypted. Keys are now stored in plaintext.");
                }
            }
        }

        Commands::TestMine { count } => {
            println!("🧪 Test mining {} blocks (in-memory)\n", count);
            let mut chain = Chain::new();
            let wallet = Wallet::new("test");
            let config = MinerConfig {
                miner_pubkey_hash: wallet.primary_pubkey_hash(),
                community_fund_hash: [0xCF; 32],
                threads: num_cpus::get().max(1),
            };
            let start = std::time::Instant::now();
            for i in 0..count {
                let stop = Arc::new(AtomicBool::new(false));
                let tpl = miner::create_block_template(&chain, &[], &config);
                match miner::mine_block_parallel(tpl, config.threads, stop) {
                    miner::MineResult::Found(block) => {
                        let h = hex::encode(block.header.hash());
                        match chain.add_block(block) {
                            Ok(_) => println!("  ✅ #{}: {} (diff {:.1})", i+1, h, chain.fractional_difficulty()),
                            Err(e) => println!("  ❌ #{}: {}", i+1, e),
                        }
                    }
                    miner::MineResult::Cancelled => break,
                }
            }
            let el = start.elapsed();
            let bal = chain.utxo_set.balance_of(&wallet.primary_pubkey_hash());
            println!("\n  {} blocks | {:.1}s | avg {:.1}s | {} EQF | diff {:.1}",
                chain.height, el.as_secs_f64(), el.as_secs_f64() / chain.height.max(1) as f64,
                format_eqf(bal), chain.fractional_difficulty());
        }
    }
}

fn open_chain(data_dir: &str) -> Chain {
    std::fs::create_dir_all(data_dir).unwrap();
    Chain::open(data_dir).unwrap_or_else(|e| { eprintln!("❌ {}", e); std::process::exit(1); })
}

use equiforge::core::types::{OutPoint, TxOutput};

// ─── Node ───────────────────────────────────────────────────────────

async fn run_node(data_dir: &str, port: u16, seeds: Vec<String>, mine: bool, threads: usize, pw: Option<&str>) {
    let state = NodeState::open(data_dir, port);
    let wallet = load_wallet(data_dir, pw);

    let (height, tip, _, _) = network::get_node_info(&state).await;
    println!("🚀 EquiForge Node v{}", PROTOCOL_VERSION);
    println!("  Data:      {}", data_dir);
    println!("  P2P:       0.0.0.0:{}", port);
    println!("  RPC:       127.0.0.1:{}", rpc_port(port));
    println!("  Explorer:  http://127.0.0.1:{}", rpc_port(port));
    println!("  Chain:     height={} tip={}", height, &hex::encode(tip)[..16]);
    println!("  Wallet:    {}", wallet.primary_address());
    println!("  Encrypted: {}", wallet.is_encrypted());
    println!("  Mining:    {}", if mine { "enabled" } else { "disabled" });
    if !SEED_NODES.is_empty() { println!("  Seeds:     {} hardcoded", SEED_NODES.len()); }

    // Load pending tx
    let pending_path = PathBuf::from(data_dir).join("pending_tx.json");
    if pending_path.exists() {
        if let Ok(json) = std::fs::read_to_string(&pending_path) {
            if let Ok(tx) = serde_json::from_str::<equiforge::core::types::Transaction>(&json) {
                let chain = state.chain.read().await;
                let mut mempool = state.mempool.lock().await;
                match mempool.validate_and_add(tx.clone(), &chain) {
                    Ok(txid) => tracing::info!("📝 Loaded pending tx: {}", hex::encode(txid)),
                    Err(e) => tracing::warn!("⚠️  Pending tx invalid: {}", e),
                }
                let _ = std::fs::remove_file(&pending_path);
            }
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    ctrlc::set_handler(move || {
        println!("\n🛑 Shutting down gracefully...");
        stop_clone.store(true, Ordering::SeqCst);
    }).expect("Ctrl-C");

    // RPC
    { let s = state.clone(); let rp = rpc_port(port);
      tokio::spawn(async move { rpc::start_rpc_server(s, rp).await; }); }

    // Mining
    if mine {
        let s = state.clone(); let st = stop.clone();
        let t = if threads == 0 { num_cpus::get().max(1) } else { threads };
        println!("  Threads:   {}", t);
        tokio::spawn(async move { mining_task(s, wallet, t, st).await; });
    }

    // Status
    { let s = state.clone(); let st = stop.clone();
      tokio::spawn(async move { status_task(s, st).await; }); }

    // Graceful shutdown watcher
    let state_for_shutdown = state.clone();
    let data_dir_owned = data_dir.to_string();
    let stop_for_shutdown = stop.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if stop_for_shutdown.load(Ordering::Relaxed) {
                // Flush storage
                tracing::info!("💾 Flushing chain to disk...");
                let chain = state_for_shutdown.chain.read().await;
                if chain.is_persistent() {
                    // Storage is flushed on every block add, but do a final flush
                    tracing::info!("💾 Chain flushed. height={} tip={}", chain.height, &hex::encode(chain.tip)[..16]);
                }
                drop(chain);

                // Log final state
                let (h, t, u, p) = network::get_node_info(&state_for_shutdown).await;
                tracing::info!("📊 Final state: height={} utxos={} peers={}", h, u, p);
                tracing::info!("👋 Shutdown complete.");

                // Give a moment for logs to flush, then exit
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                std::process::exit(0);
            }
        }
    });

    // P2P
    if let Err(e) = network::start_node(state, seeds).await {
        tracing::error!("Node error: {}", e);
    }
}

async fn mining_task(state: Arc<NodeState>, wallet: Wallet, threads: usize, stop: Arc<AtomicBool>) {
    tracing::info!("⛏️  Mining to {}", wallet.primary_address());
    loop {
        if stop.load(Ordering::Relaxed) { break; }
        let tpl = {
            let chain = state.chain.read().await;
            let mp = state.mempool.lock().await;
            let pending = mp.get_pending();
            drop(mp);
            let cfg = MinerConfig {
                miner_pubkey_hash: wallet.primary_pubkey_hash(),
                community_fund_hash: [0xCF; 32], threads,
            };
            miner::create_block_template(&chain, &pending, &cfg)
        };

        let ms = Arc::new(AtomicBool::new(false));
        let ms2 = ms.clone(); let gs = stop.clone();
        let w = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if gs.load(Ordering::Relaxed) { ms2.store(true, Ordering::Relaxed); break; }
            }
        });

        let result = tokio::task::spawn_blocking(move || {
            miner::mine_block_parallel(tpl, threads, ms)
        }).await.unwrap();
        w.abort();

        match result {
            miner::MineResult::Found(block) => network::broadcast_block(&state, block).await,
            miner::MineResult::Cancelled => { if stop.load(Ordering::Relaxed) { break; } }
        }
    }
}

async fn status_task(state: Arc<NodeState>, stop: Arc<AtomicBool>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        if stop.load(Ordering::Relaxed) { break; }
        let (h, tip, u, p) = network::get_node_info(&state).await;
        let fd = state.chain.read().await.fractional_difficulty();
        let bans = state.scoreboard.lock().await.ban_count();
        tracing::info!("📊 height={} diff={:.1} tip={} utxos={} peers={} banned={}",
            h, fd, &hex::encode(tip)[..16], u, p, bans);
    }
}
