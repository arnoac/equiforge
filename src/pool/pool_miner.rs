//! EquiForge Pool Miner — Lightweight Mining Client
//!
//! Connects to EquiForge pool servers and mines using EquiHash-X.
//! No full node, blockchain, or wallet file needed.
//!
//! Features:
//!   - Multiple pool addresses: probes latency, picks the fastest
//!   - Auto-failover: if current pool dies, tries next-best immediately
//!   - Periodic re-probe: detects recovered pools, can switch back
//!
//! Usage:
//!   equiforge pool-mine --pool 1.2.3.4:9334 --pool 5.6.7.8:9334 --address <hash> -t 4

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpStream;

use super::{read_pool_msg, write_pool_msg, PoolMessage};
use crate::core::types::{leading_zero_bits, BlockHeader};
use crate::pow;

// ─── Mining ─────────────────────────────────────────────────────────

struct MiningJob {
    job_id: u64,
    header: BlockHeader,
    share_target: u32,
    network_target: u32,
}

/// Nonce offset — seeded randomly on first use so each process start
/// explores different nonce space. Prevents "duplicate nonce" on reconnect.
static NONCE_OFFSET: AtomicU64 = AtomicU64::new(0);
static NONCE_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn init_nonce_offset() {
    if !NONCE_INITIALIZED.swap(true, Ordering::Relaxed) {
        // Mix timestamp nanos + PID for a cheap unique seed (no rand crate needed)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let seed = now.as_nanos() as u64 ^ (std::process::id() as u64 * 6364136223846793005);
        NONCE_OFFSET.store(seed, Ordering::Relaxed);
    }
}

fn mine_job(
    job: &MiningJob,
    threads: usize,
    stop: Arc<AtomicBool>,
) -> Option<(u64, [u8; 32])> {
    init_nonce_offset();
    let offset = NONCE_OFFSET.fetch_add(1_000_000_000, Ordering::Relaxed);
    let nonce_range = u64::MAX / threads as u64;
    let (tx, rx) = std::sync::mpsc::channel();

    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let mut header = job.header.clone();
            let share_target = job.share_target;
            let stop = stop.clone();
            let tx = tx.clone();
            let base = (i as u64).wrapping_mul(nonce_range);
            let start = base.wrapping_add(offset);

            std::thread::spawn(move || {
                let mut nonce = start;
                let mut count: u64 = 0;
                loop {
                    if stop.load(Ordering::Relaxed) { return; }
                    header.nonce = nonce;
                    let serialized = bincode::serialize(&header).expect("serialize");
                    let hash = pow::equihash_x(&serialized);
                    if leading_zero_bits(&hash) >= share_target {
                        let _ = tx.send((nonce, hash));
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                    nonce = nonce.wrapping_add(1);
                    count += 1;
                    if count >= nonce_range { return; }
                }
            })
        })
        .collect();

    drop(tx);
    let result = rx.recv().ok();
    stop.store(true, Ordering::Relaxed);
    for h in handles { let _ = h.join(); }
    result
}

// ─── Pool Probing ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PoolProbe {
    addr: String,
    latency_ms: u64,
    reachable: bool,
}

/// Probe all pool addresses concurrently via TCP connect.
/// Returns sorted by latency (best first), unreachable at end.
async fn probe_pools(addrs: &[String]) -> Vec<PoolProbe> {
    let mut handles = Vec::new();
    for addr in addrs {
        let addr = addr.clone();
        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                TcpStream::connect(&addr),
            ).await {
                Ok(Ok(stream)) => {
                    let ms = start.elapsed().as_millis() as u64;
                    drop(stream);
                    PoolProbe { addr, latency_ms: ms, reachable: true }
                }
                _ => PoolProbe { addr, latency_ms: u64::MAX, reachable: false },
            }
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        if let Ok(p) = h.await { results.push(p); }
    }
    results.sort_by(|a, b| match (a.reachable, b.reachable) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.latency_ms.cmp(&b.latency_ms),
    });
    results
}

fn print_probes(probes: &[PoolProbe]) {
    println!("🌐 Pool latency probe:");
    for (i, p) in probes.iter().enumerate() {
        if p.reachable {
            println!("   {:>4}ms  {}{}", p.latency_ms, p.addr, if i == 0 { " ← best" } else { "" });
        } else {
            println!("    ---   {} (unreachable)", p.addr);
        }
    }
    println!();
}

// ─── Config & Entry Point ───────────────────────────────────────────

pub struct PoolMinerConfig {
    /// One or more pool server addresses.
    /// Miner probes latency and picks the best. Falls back on disconnect.
    pub pool_addrs: Vec<String>,
    pub worker_name: String,
    pub payout_address: String,
    pub threads: usize,
}

pub async fn run_pool_miner(config: PoolMinerConfig) {
    println!("⛏️  EquiForge Pool Miner");
    println!("   Pools:   {} configured", config.pool_addrs.len());
    for addr in &config.pool_addrs {
        println!("            - {}", addr);
    }
    println!("   Worker:  {}", config.worker_name);
    println!("   Payout:  {}…", &config.payout_address[..16.min(config.payout_address.len())]);
    println!("   Threads: {}", config.threads);
    println!();

    let mut consecutive_failures: u32 = 0;

    loop {
        // ── Probe all pools ──
        let probes = probe_pools(&config.pool_addrs).await;
        print_probes(&probes);

        let reachable: Vec<&PoolProbe> = probes.iter().filter(|p| p.reachable).collect();

        if reachable.is_empty() {
            let delay = (2u64.pow(consecutive_failures.min(6))).min(60);
            eprintln!("❌ No reachable pools. Retrying in {}s...", delay);
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            consecutive_failures += 1;
            continue;
        }

        // ── Try each reachable pool in latency order ──
        let mut any_success = false;
        for probe in &reachable {
            println!("🔗 Connecting to {} ({}ms latency)...", probe.addr, probe.latency_ms);

            match connect_and_mine(&probe.addr, &config).await {
                Ok(()) => {
                    // Clean disconnect (pool shut down gracefully).
                    // Re-probe to find another pool.
                    println!("📡 Pool {} closed connection. Re-probing...", probe.addr);
                    consecutive_failures = 0;
                    any_success = true;
                    break;
                }
                Err(e) => {
                    eprintln!("❌ {} — {}", probe.addr, e);
                    // Try next pool immediately (no delay between failover attempts)
                    println!("🔄 Failing over to next pool...");
                    continue;
                }
            }
        }

        if !any_success {
            consecutive_failures += 1;
        }

        // Pause before re-probing
        let delay = if consecutive_failures > 0 {
            (2u64.pow(consecutive_failures.min(6))).min(60)
        } else {
            2
        };
        println!("🔄 Re-probing pools in {}s...\n", delay);
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }
}

// ─── Single-Pool Mining Session ─────────────────────────────────────

async fn connect_and_mine(pool_addr: &str, config: &PoolMinerConfig) -> Result<(), String> {
    let mut stream = TcpStream::connect(pool_addr)
        .await
        .map_err(|e| format!("connect: {}", e))?;
    let _ = stream.set_nodelay(true);
    println!("✅ Connected to {}", pool_addr);

    // Register
    write_pool_msg(&mut stream, &PoolMessage::Register {
        worker_name: config.worker_name.clone(),
        payout_address: config.payout_address.clone(),
    }).await?;

    let mut current_job: Option<MiningJob> = None;
    let mut total_shares: u64 = 0;
    let session_start = Instant::now();

    loop {
        if let Some(ref job) = current_job {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_mine = stop.clone();
            let stop_cancel = stop.clone();

            let job_id = job.job_id;
            let share_target = job.share_target;
            let network_target = job.network_target;
            let height = job.header.height;
            let threads = config.threads;
            let mining_job = MiningJob {
                job_id, header: job.header.clone(), share_target, network_target,
            };

            let mine_handle = tokio::task::spawn_blocking(move || {
                mine_job(&mining_job, threads, stop_mine)
            });

            tokio::select! {
                mine_result = mine_handle => {
                    match mine_result {
                        Ok(Some((nonce, hash))) => {
                            let zeros = leading_zero_bits(&hash);
                            total_shares += 1;
                            if zeros >= network_target {
                                println!("🎉 BLOCK FOUND! height={} hash={} nonce={}", height, hex::encode(hash), nonce);
                            } else {
                                println!("📤 Share #{}: nonce={} zeros={}/{}", total_shares, nonce, zeros, share_target);
                            }

                            write_pool_msg(&mut stream, &PoolMessage::SubmitShare { job_id, nonce }).await?;

                            match tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                read_pool_msg(&mut stream),
                            ).await {
                                Ok(Ok(PoolMessage::ShareAccepted { shares_accepted, hashrate_estimate })) => {
                                    let elapsed = session_start.elapsed().as_secs_f64();
                                    println!("✅ Accepted (pool total: {}, est: {:.1} H/s, session: {:.0}s)",
                                        shares_accepted, hashrate_estimate, elapsed);
                                }
                                Ok(Ok(PoolMessage::ShareRejected { reason })) => {
                                    println!("❌ Rejected: {}", reason);
                                }
                                Ok(Ok(PoolMessage::BlockFound { height, hash, finder })) => {
                                    println!("🎉 Block #{} by {}! ({}…)", height, finder, &hash[..16.min(hash.len())]);
                                    drain_until_job(&mut stream, &mut current_job).await?;
                                }
                                Ok(Ok(PoolMessage::JobCancel)) => {
                                    println!("🔄 Job cancelled");
                                    current_job = None;
                                    drain_until_job(&mut stream, &mut current_job).await?;
                                }
                                Ok(Ok(PoolMessage::Job { job_id, header, share_target, network_target })) => {
                                    println!("📋 Job #{}: height={} diff={}/{}", job_id, header.height, share_target, network_target);
                                    current_job = Some(MiningJob { job_id, header, share_target, network_target });
                                }
                                Ok(Ok(_)) => {}
                                Ok(Err(e)) => return Err(e),
                                Err(_) => println!("⚠️  Share response timeout"),
                            }
                            continue;
                        }
                        Ok(None) => continue,
                        Err(e) => return Err(format!("mining thread panic: {}", e)),
                    }
                }

                msg = read_pool_msg(&mut stream) => {
                    stop_cancel.store(true, Ordering::Relaxed);
                    match msg {
                        Ok(PoolMessage::JobCancel) => {
                            println!("🔄 Job cancelled");
                            current_job = None;
                            drain_until_job(&mut stream, &mut current_job).await?;
                        }
                        Ok(PoolMessage::Job { job_id, header, share_target, network_target }) => {
                            println!("📋 Job #{}: height={} diff={}/{}", job_id, header.height, share_target, network_target);
                            current_job = Some(MiningJob { job_id, header, share_target, network_target });
                        }
                        Ok(PoolMessage::BlockFound { height, hash, finder }) => {
                            println!("🎉 Block #{} by {}! ({}…)", height, finder, &hash[..16.min(hash.len())]);
                        }
                        Ok(PoolMessage::PoolStats { connected_miners, pool_hashrate, blocks_found, current_height }) => {
                            println!("📊 Pool: {} miners, {:.1} H/s, {} blocks, height {}",
                                connected_miners, pool_hashrate, blocks_found, current_height);
                        }
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                read_pool_msg(&mut stream),
            ).await {
                Ok(Ok(PoolMessage::Job { job_id, header, share_target, network_target })) => {
                    println!("📋 Job #{}: height={} diff={}/{}", job_id, header.height, share_target, network_target);
                    current_job = Some(MiningJob { job_id, header, share_target, network_target });
                }
                Ok(Ok(PoolMessage::PoolStats { connected_miners, pool_hashrate, blocks_found, current_height })) => {
                    println!("📊 Pool: {} miners, {:.1} H/s, {} blocks, height {}",
                        connected_miners, pool_hashrate, blocks_found, current_height);
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => println!("⏳ Waiting for job from {}...", pool_addr),
            }
        }
    }
}

async fn drain_until_job(
    stream: &mut TcpStream,
    current_job: &mut Option<MiningJob>,
) -> Result<(), String> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_pool_msg(stream),
    ).await {
        Ok(Ok(PoolMessage::Job { job_id, header, share_target, network_target })) => {
            println!("📋 Job #{}: height={} diff={}/{}", job_id, header.height, share_target, network_target);
            *current_job = Some(MiningJob { job_id, header, share_target, network_target });
        }
        Ok(Ok(PoolMessage::JobCancel)) => { *current_job = None; }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => { *current_job = None; }
    }
    Ok(())
}