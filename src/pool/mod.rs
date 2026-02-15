//! EquiForge Mining Pool
//!
//! Architecture:
//!   Pool Miners (CPU only) ←TCP→ Pool Server ←Arc<NodeState>→ Full Node
//!
//! The pool server creates block templates with the pool operator's address,
//! distributes headers to workers, validates shares, and submits found blocks.
//! Pool miners need only this protocol + the PoW function — no blockchain.

pub mod pool_miner;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

use crate::core::params::*;
use crate::core::types::*;
use crate::miner;
use crate::network::{self, NodeState};

// ═══════════════════════════════════════════════════════════════════
// Shared protocol — used by BOTH pool server and pool_miner client.
// pool_miner.rs imports these via `use super::*`.
// ═══════════════════════════════════════════════════════════════════

/// Messages between pool server and pool miners.
/// Wire format: [4-byte length LE][bincode payload]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PoolMessage {
    // ── Miner → Server ──
    /// Register with the pool. payout_address is hex-encoded 32-byte pubkey hash.
    Register {
        worker_name: String,
        payout_address: String,
    },
    /// Submit a nonce that meets share_target.
    SubmitShare {
        job_id: u64,
        nonce: u64,
    },

    // ── Server → Miner ──
    /// New mining job.
    Job {
        job_id: u64,
        /// Block header template — miner overwrites nonce and hashes.
        header: BlockHeader,
        /// Minimum leading zero bits for a valid share.
        share_target: u32,
        /// Actual network difficulty — hash meeting this is a real block.
        network_target: u32,
    },
    /// Current job cancelled — stop mining, wait for next Job.
    JobCancel,
    /// Share accepted.
    ShareAccepted {
        shares_accepted: u64,
        hashrate_estimate: f64,
    },
    /// Share rejected.
    ShareRejected {
        reason: String,
    },
    /// A real block was found by a pool miner.
    BlockFound {
        height: u64,
        hash: String,
        finder: String,
    },
    /// Periodic pool stats.
    PoolStats {
        connected_miners: u32,
        pool_hashrate: f64,
        blocks_found: u64,
        current_height: u64,
    },
}

const MAX_POOL_MSG: usize = 1024 * 1024;

pub async fn read_pool_msg(stream: &mut TcpStream) -> Result<PoolMessage, String> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.map_err(|e| format!("read len: {}", e))?;
    let length = u32::from_le_bytes(len_buf) as usize;
    if length > MAX_POOL_MSG {
        return Err("message too large".into());
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await.map_err(|e| format!("read payload: {}", e))?;
    bincode::deserialize(&payload).map_err(|e| format!("deserialize: {}", e))
}

pub async fn write_pool_msg(stream: &mut TcpStream, msg: &PoolMessage) -> Result<(), String> {
    let payload = bincode::serialize(msg).map_err(|e| format!("serialize: {}", e))?;
    let len_bytes = (payload.len() as u32).to_le_bytes();
    stream.write_all(&len_bytes).await.map_err(|e| format!("write len: {}", e))?;
    stream.write_all(&payload).await.map_err(|e| format!("write payload: {}", e))?;
    stream.flush().await.map_err(|e| format!("flush: {}", e))?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Pool Server internals (only runs on the node)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct Worker {
    name: String,
    payout_hash: Hash256,
    shares_accepted: u64,
    shares_submitted: u64,
    connected_at: u64,
    recent_share_times: Vec<u64>,
}

impl Worker {
    fn hashrate_estimate(&self, share_diff: u32) -> f64 {
        let buf = &self.recent_share_times;
        if buf.len() < 2 {
            return 0.0;
        }
        let window = buf.len().min(30);
        let recent = &buf[buf.len() - window..];
        let elapsed = recent.last().unwrap().saturating_sub(*recent.first().unwrap());
        if elapsed == 0 {
            return 0.0;
        }
        let hashes_per_share = (1u64 << share_diff.min(63)) as f64;
        (window as f64 * hashes_per_share) / elapsed as f64
    }

    fn record_share(&mut self, now: u64) {
        self.shares_accepted += 1;
        self.recent_share_times.push(now);
        if self.recent_share_times.len() > 120 {
            self.recent_share_times.drain(0..self.recent_share_times.len() - 120);
        }
    }
}

// ─── Pool Configuration ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub port: u16,
    pub fee_percent: f64,
    pub share_diff_offset: u32,
    pub min_share_difficulty: u32,
    pub pplns_window: usize,
    pub pool_payout_hash: Hash256,
    pub pool_name: String,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            port: 9334,
            fee_percent: 1.0,
            share_diff_offset: 4,
            min_share_difficulty: 4,
            pplns_window: 10_000,
            pool_payout_hash: [0xFE; 32],
            pool_name: String::from("EquiForge-Pool"),
        }
    }
}

// ─── Pool State ─────────────────────────────────────────────────────

struct PoolState {
    config: PoolConfig,
    workers: HashMap<String, Worker>,
    job_id: u64,
    /// FULL block template — header + txs. When a winning nonce is found,
    /// we clone this, set the nonce, and submit. No re-creation needed.
    current_template: Option<Block>,
    network_target: u32,
    share_target: u32,
    used_nonces: std::collections::HashSet<u64>,
    pplns_window: Vec<(String, Hash256)>,
    blocks_found: u64,
}

impl PoolState {
    fn new(config: PoolConfig) -> Self {
        Self {
            config,
            workers: HashMap::new(),
            job_id: 0,
            current_template: None,
            network_target: 0,
            share_target: 0,
            used_nonces: std::collections::HashSet::new(),
            pplns_window: Vec::new(),
            blocks_found: 0,
        }
    }

    fn compute_share_target(&self, network_diff: u32) -> u32 {
        network_diff
            .saturating_sub(self.config.share_diff_offset)
            .max(self.config.min_share_difficulty)
    }

    fn record_share(&mut self, worker_name: &str, payout_hash: Hash256) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        if let Some(w) = self.workers.get_mut(worker_name) {
            w.record_share(now);
        }
        self.pplns_window.push((worker_name.to_string(), payout_hash));
        if self.pplns_window.len() > self.config.pplns_window {
            let excess = self.pplns_window.len() - self.config.pplns_window;
            self.pplns_window.drain(0..excess);
        }
    }

    fn pool_hashrate(&self) -> f64 {
        self.workers
            .values()
            .map(|w| w.hashrate_estimate(self.share_target))
            .sum()
    }
}

// ─── Pool Server Entry Point ────────────────────────────────────────

pub async fn start_pool_server(
    node_state: Arc<NodeState>,
    config: PoolConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("⛏️  Pool server on port {}", config.port);
    tracing::info!(
        "    Fee: {}%  |  Share offset: -{} bits  |  PPLNS window: {}",
        config.fee_percent, config.share_diff_offset, config.pplns_window
    );

    let pool = Arc::new(RwLock::new(PoolState::new(config)));

    // Create initial job template
    refresh_template(&node_state, &pool).await;

    // Job updater — watches for new blocks
    {
        let ns = node_state.clone();
        let p = pool.clone();
        tokio::spawn(async move {
            loop {
                ns.new_block_notify.notified().await;
                refresh_template(&ns, &p).await;
            }
        });
    }

    // Stats logger
    {
        let ns = node_state.clone();
        let p = pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let ps = p.read().await;
                let h = ns.chain.read().await.height;
                tracing::info!(
                    "⛏️  Pool: {} miners, {:.1} H/s, {} blocks found, chain height {}",
                    ps.workers.len(),
                    ps.pool_hashrate(),
                    ps.blocks_found,
                    h
                );
            }
        });
    }

    // Accept miner connections
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let ns = node_state.clone();
                let p = pool.clone();
                let peer = addr.to_string();
                tokio::spawn(async move {
                    handle_worker(stream, peer, ns, p).await;
                });
            }
            Err(e) => tracing::error!("Pool accept error: {}", e),
        }
    }
}

/// Build a fresh block template and store it in pool state.
async fn refresh_template(node_state: &Arc<NodeState>, pool: &Arc<RwLock<PoolState>>) {
    let chain = node_state.chain.read().await;
    let mp = node_state.mempool.lock().await;
    let pending = mp.get_pending();
    drop(mp);

    let network_diff = chain.next_difficulty();
    let pool_hash = pool.read().await.config.pool_payout_hash;
    let miner_cfg = miner::MinerConfig {
        miner_pubkey_hash: pool_hash,
        community_fund_hash: [0xCF; 32],
        threads: 1,
        miner_tag: format!("pool:{}", pool.read().await.config.pool_name),
    };
    let template = miner::create_block_template(&chain, &pending, &miner_cfg);
    let height = template.header.height;
    drop(chain);

    let mut ps = pool.write().await;
    ps.job_id += 1;
    ps.network_target = network_diff;
    ps.share_target = ps.compute_share_target(network_diff);
    ps.current_template = Some(template);
    ps.used_nonces.clear();

    tracing::info!(
        "⛏️  New pool job #{}: height={} net_diff={} share_diff={}",
        ps.job_id, height, network_diff, ps.share_target
    );
}

fn make_job_msg(ps: &PoolState) -> Option<PoolMessage> {
    ps.current_template.as_ref().map(|tpl| PoolMessage::Job {
        job_id: ps.job_id,
        header: tpl.header.clone(),
        share_target: ps.share_target,
        network_target: ps.network_target,
    })
}

// ─── Per-Worker Handler ─────────────────────────────────────────────

async fn handle_worker(
    mut stream: TcpStream,
    peer: String,
    node_state: Arc<NodeState>,
    pool: Arc<RwLock<PoolState>>,
) {
    let _ = stream.set_nodelay(true);

    // ── Registration ──
    let (name, payout_hash) = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_pool_msg(&mut stream),
    )
    .await
    {
        Ok(Ok(PoolMessage::Register {
            worker_name,
            payout_address,
        })) => match hex::decode(&payout_address) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut h = [0u8; 32];
                h.copy_from_slice(&bytes);
                tracing::info!(
                    "⛏️  Worker '{}' registered from {} (payout: {}…)",
                    worker_name,
                    peer,
                    &payout_address[..16]
                );
                (worker_name, h)
            }
            _ => {
                let _ = write_pool_msg(
                    &mut stream,
                    &PoolMessage::ShareRejected {
                        reason: "bad payout address (need 64 hex chars)".into(),
                    },
                )
                .await;
                return;
            }
        },
        _ => {
            return;
        }
    };

    // Add worker
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut ps = pool.write().await;
        ps.workers.insert(
            name.clone(),
            Worker {
                name: name.clone(),
                payout_hash,
                shares_accepted: 0,
                shares_submitted: 0,
                connected_at: now,
                recent_share_times: Vec::new(),
            },
        );
    }

    // Send initial job
    {
        let ps = pool.read().await;
        if let Some(job) = make_job_msg(&ps) {
            let _ = write_pool_msg(&mut stream, &job).await;
        }
    }

    // Subscribe to block broadcast for job updates
    let mut block_rx = node_state.block_tx.subscribe();

    // ── Main loop ──
    loop {
        tokio::select! {
            // New block → cancel + send fresh job
            _ = block_rx.recv() => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = write_pool_msg(&mut stream, &PoolMessage::JobCancel).await;
                let ps = pool.read().await;
                if let Some(job) = make_job_msg(&ps) {
                    let _ = write_pool_msg(&mut stream, &job).await;
                }
            }

            // Message from miner
            msg = tokio::time::timeout(
                std::time::Duration::from_secs(300),
                read_pool_msg(&mut stream),
            ) => {
                match msg {
                    Ok(Ok(PoolMessage::SubmitShare { job_id, nonce })) => {
                        process_share(
                            &mut stream, &name, payout_hash,
                            job_id, nonce, &node_state, &pool,
                        ).await;
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::debug!("Worker '{}' error: {}", name, e);
                        break;
                    }
                    Err(_) => {
                        tracing::debug!("Worker '{}' timed out", name);
                        break;
                    }
                }
            }
        }
    }

    pool.write().await.workers.remove(&name);
    tracing::info!("⛏️  Worker '{}' disconnected", name);
}

// ─── Share Processing ───────────────────────────────────────────────

async fn process_share(
    stream: &mut TcpStream,
    worker_name: &str,
    payout_hash: Hash256,
    job_id: u64,
    nonce: u64,
    node_state: &Arc<NodeState>,
    pool: &Arc<RwLock<PoolState>>,
) {
    // Take a snapshot of what we need under a read lock
    let (header, share_target, network_target, current_job_id) = {
        let ps = pool.read().await;
        match ps.current_template {
            Some(ref tpl) => (
                tpl.header.clone(),
                ps.share_target,
                ps.network_target,
                ps.job_id,
            ),
            None => {
                let _ = write_pool_msg(
                    stream,
                    &PoolMessage::ShareRejected {
                        reason: "no active job".into(),
                    },
                )
                .await;
                return;
            }
        }
    };

    // Stale?
    if job_id != current_job_id {
        tracing::debug!(
            "Stale share from '{}': job {} vs current {}",
            worker_name, job_id, current_job_id
        );
        let _ = write_pool_msg(
            stream,
            &PoolMessage::ShareRejected {
                reason: format!("stale job (yours={}, current={})", job_id, current_job_id),
            },
        )
        .await;
        return;
    }

    // Duplicate nonce?
    {
        let mut ps = pool.write().await;
        if let Some(w) = ps.workers.get_mut(worker_name) {
            w.shares_submitted += 1;
        }
        if !ps.used_nonces.insert(nonce) {
            tracing::warn!(
                "Duplicate nonce from '{}': nonce={} job={} (set size={})",
                worker_name, nonce, job_id, ps.used_nonces.len()
            );
            let _ = write_pool_msg(
                stream,
                &PoolMessage::ShareRejected {
                    reason: "duplicate nonce".into(),
                },
            )
            .await;
            return;
        }
    }

    // Verify PoW
    let mut check = header.clone();
    check.nonce = nonce;
    let hash = check.hash();
    let zeros = leading_zero_bits(&hash);

    if zeros < share_target {
        tracing::debug!(
            "Bad share from '{}': nonce={} zeros={} need={}",
            worker_name, nonce, zeros, share_target
        );
        let _ = write_pool_msg(
            stream,
            &PoolMessage::ShareRejected {
                reason: format!(
                    "insufficient PoW: {} zeros < {} required",
                    zeros, share_target
                ),
            },
        )
        .await;
        return;
    }

    // ── Valid share ──
    let (accepted, hashrate) = {
        let mut ps = pool.write().await;
        ps.record_share(worker_name, payout_hash);
        let w = ps.workers.get(worker_name);
        let acc = w.map(|w| w.shares_accepted).unwrap_or(0);
        let hr = w
            .map(|w| w.hashrate_estimate(ps.share_target))
            .unwrap_or(0.0);
        (acc, hr)
    };

    let _ = write_pool_msg(
        stream,
        &PoolMessage::ShareAccepted {
            shares_accepted: accepted,
            hashrate_estimate: hashrate,
        },
    )
    .await;

    tracing::info!(
        "⛏️  Share OK from '{}': nonce={} zeros={} (accepted: {})",
        worker_name, nonce, zeros, accepted
    );

    // ── Check if it's a REAL BLOCK ──
    if zeros >= network_target {
        tracing::info!(
            "🎉 BLOCK FOUND by '{}'! height={} hash={}",
            worker_name,
            check.height,
            hex::encode(hash)
        );

        let block = {
            let ps = pool.read().await;
            ps.current_template.as_ref().map(|tpl| {
                let mut block = tpl.clone();
                block.header.nonce = nonce;
                block
            })
        };

        if let Some(block) = block {
            let block_hash = block.header.hash();
            if block_hash == hash {
                network::broadcast_block(node_state, block).await;

                let mut ps = pool.write().await;
                ps.blocks_found += 1;
                tracing::info!(
                    "🎉 Pool block #{} submitted! Lifetime total: {}",
                    check.height,
                    ps.blocks_found
                );

                let _ = write_pool_msg(
                    stream,
                    &PoolMessage::BlockFound {
                        height: check.height,
                        hash: hex::encode(block_hash),
                        finder: worker_name.to_string(),
                    },
                )
                .await;
            } else {
                tracing::warn!(
                    "Block hash mismatch (stale template) — share valid, block discarded"
                );
            }
        }
    }
}