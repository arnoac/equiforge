use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::core::chain::Chain;
use crate::core::params::*;
use crate::core::types::*;

// ─── Message Types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetMessage {
    Version { version: u32, best_height: u64, best_hash: Hash256, timestamp: u64, listen_port: u16 },
    VersionAck,
    NewBlock(Block),
    NewTransaction(Transaction),
    GetBlocks { start_height: u64, count: u32 },
    Blocks(Vec<Block>),
    GetBlock(Hash256),
    Ping(u64),
    Pong(u64),
    GetPeers,
    Peers(Vec<String>),
    VersionV2 { version: u32, best_height: u64, best_hash: Hash256, genesis_hash: Hash256, timestamp: u64, listen_port: u16 },
    // ─── Headers-first sync ───
    GetHeaders { start_height: u64, count: u32 },
    GetHeadersFrom { locator: Vec<Hash256>, count: u32 },
    Headers(Vec<BlockHeader>),
    GetBlockData(Vec<Hash256>),  // Request full blocks by hash after header validation
    BlockData(Vec<Block>),
    // ─── Compact block relay ───
    CompactBlock { header: BlockHeader, short_txids: Vec<Hash256>, coinbase: Transaction },
    GetTransactions(Vec<Hash256>), // Request missing txs for compact block
    TransactionBatch(Vec<Transaction>),
}


#[derive(Debug, Clone)]
struct PendingCompact {
    header: BlockHeader,
    txs: Vec<Option<Transaction>>,   // index 0 is coinbase
    /// txid -> index in `txs`
    index_map: HashMap<Hash256, usize>,
    missing: std::collections::HashSet<Hash256>,
    created_at: std::time::Instant,
}


// ─── Wire Protocol ───────────────────────────────────────────────────

const HEADER_SIZE: usize = 8;
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

pub fn encode_message(msg: &NetMessage) -> Vec<u8> {
    let payload = bincode::serialize(msg).expect("serialization failed");
    let mut data = Vec::with_capacity(HEADER_SIZE + payload.len());
    data.extend_from_slice(&magic_bytes());
    data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    data.extend_from_slice(&payload);
    data
}

async fn read_message(stream: &mut TcpStream) -> Result<NetMessage, String> {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.map_err(|e| format!("read header: {}", e))?;
    if header[0..4] != magic_bytes() { return Err("invalid magic bytes".into()); }
    let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    if length > MAX_MESSAGE_SIZE { return Err(format!("message too large: {} bytes", length)); }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await.map_err(|e| format!("read payload: {}", e))?;
    bincode::deserialize(&payload).map_err(|e| format!("deserialize: {}", e))
}

async fn write_message(stream: &mut TcpStream, msg: &NetMessage) -> Result<(), String> {
    let data = encode_message(msg);
    stream.write_all(&data).await.map_err(|e| format!("write: {}", e))?;
    stream.flush().await.map_err(|e| format!("flush: {}", e))?;
    Ok(())
}

fn build_locator(chain: &Chain, max: usize) -> Vec<Hash256> {
    // Newest -> oldest, exponential backoff, always include genesis
    let mut locator = Vec::new();

    let mut h = chain.height;
    let mut step: u64 = 1;

    while locator.len() < max {
        if let Some(b) = chain.block_at_height(h) {
            locator.push(b.header.hash());
        } else {
            // safety fallback
            locator.push(chain.tip);
        }

        if h == 0 {
            break;
        }

        h = h.saturating_sub(step);

        // after a few entries, back off faster
        if locator.len() > 8 {
            step = (step * 2).min(1024);
        }
    }

    // Ensure genesis is included
    let g = chain.genesis_hash();
    if g != NULL_HASH && locator.last().copied() != Some(g) {
        locator.push(g);
    }

    locator
}


// ─── Per-Peer Rate Limiter ──────────────────────────────────────────
// TODO: Wire into handle_connection — create per-connection instance,
//       call record_send/record_recv on each message, disconnect if limited.

#[allow(dead_code)]
struct PeerRateLimiter {
    /// Bytes sent in current window
    bytes_sent: u64,
    /// Bytes received in current window
    bytes_recv: u64,
    /// Window start time
    window_start: u64,
    /// Max bytes per second (outbound)
    max_send_rate: u64,
    /// Max bytes per second (inbound)
    max_recv_rate: u64,
}

#[allow(dead_code)]
impl PeerRateLimiter {
    fn new() -> Self {
        Self {
            bytes_sent: 0, bytes_recv: 0,
            window_start: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            max_send_rate: 10 * 1024 * 1024, // 10 MB/s default
            max_recv_rate: 10 * 1024 * 1024,
        }
    }

    fn record_send(&mut self, bytes: u64) {
        self.maybe_reset_window();
        self.bytes_sent += bytes;
    }

    fn record_recv(&mut self, bytes: u64) {
        self.maybe_reset_window();
        self.bytes_recv += bytes;
    }

    fn is_send_limited(&self) -> bool {
        let elapsed = self.elapsed_secs().max(1);
        self.bytes_sent / elapsed > self.max_send_rate
    }

    fn is_recv_limited(&self) -> bool {
        let elapsed = self.elapsed_secs().max(1);
        self.bytes_recv / elapsed > self.max_recv_rate
    }

    fn elapsed_secs(&self) -> u64 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        now.saturating_sub(self.window_start)
    }

    fn maybe_reset_window(&mut self) {
        if self.elapsed_secs() >= 10 {
            self.bytes_sent = 0;
            self.bytes_recv = 0;
            self.window_start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        }
    }
}

// ─── Anchor Connections ────────────────────────────────────────────

/// Anchor connections are persistent peers that survive restarts.
/// Stored as a file in the data directory so we reconnect on restart.
const MAX_ANCHORS: usize = 4;
const ANCHOR_FILE: &str = "anchors.json";

pub fn load_anchors(data_dir: &str) -> Vec<String> {
    let path = std::path::PathBuf::from(data_dir).join(ANCHOR_FILE);
    if let Ok(data) = std::fs::read_to_string(&path) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        Vec::new()
    }
}

pub fn save_anchors(data_dir: &str, anchors: &[String]) {
    let path = std::path::PathBuf::from(data_dir).join(ANCHOR_FILE);
    let limited: Vec<&String> = anchors.iter().take(MAX_ANCHORS).collect();
    if let Ok(json) = serde_json::to_string(&limited) {
        let _ = std::fs::write(path, json);
    }
}

// ─── Ban System ─────────────────────────────────────────────────────

/// Tracks misbehavior per peer IP. After enough strikes, the peer is banned.
const BAN_THRESHOLD: u32 = 20;
/// Ban duration in seconds (30 minutes)
const BAN_DURATION: u64 = 1800;

/// Severity of different offenses
#[derive(Debug)]
enum Offense {
    InvalidBlock,       // 2 strikes — could be a stale block, not always malicious
    InvalidTransaction, // 1 strike  — could be a double-spend race
    MalformedMessage,   // 3 strikes — definitely misbehaving
    SpamPing,           // 1 strike
}

impl Offense {
    fn strikes(&self) -> u32 {
        match self {
            Offense::InvalidBlock => 2,
            Offense::InvalidTransaction => 1,
            Offense::MalformedMessage => 3,
            Offense::SpamPing => 1,
        }
    }
}

pub struct BanEntry {
    pub banned_until: u64,
    pub reason: String,
}

pub struct PeerScoreboard {
    /// Strike count per IP (not per connection, so reconnecting doesn't reset)
    strikes: HashMap<String, u32>,
    /// Banned IPs with expiry
    bans: HashMap<String, BanEntry>,
}

impl PeerScoreboard {
    pub fn new() -> Self {
        Self { strikes: HashMap::new(), bans: HashMap::new() }
    }

    /// Extract the IP portion from a "IP:port" address string
    fn ip_of(addr: &str) -> String {
        addr.split(':').next().unwrap_or(addr).to_string()
    }

    /// Record an offense. Returns true if the peer should be banned.
    pub fn record_offense(&mut self, addr: &str, offense: Offense) -> bool {
        let ip = Self::ip_of(addr);
        let count = self.strikes.entry(ip.clone()).or_insert(0);
        *count += offense.strikes();

        if *count >= BAN_THRESHOLD {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            tracing::warn!("🚫 Banning {} for {} strikes (last: {:?})", ip, count, offense);
            self.bans.insert(ip, BanEntry {
                banned_until: now + BAN_DURATION,
                reason: format!("{:?}", offense),
            });
            return true;
        }
        false
    }

    /// Check if an IP is currently banned
    pub fn is_banned(&self, addr: &str) -> bool {
        let ip = Self::ip_of(addr);
        if let Some(entry) = self.bans.get(&ip) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            return now < entry.banned_until;
        }
        false
    }

    /// Clean up expired bans and reset strikes for unbanned IPs
    pub fn cleanup(&mut self) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let expired: Vec<String> = self.bans.iter()
            .filter(|(_, e)| now >= e.banned_until)
            .map(|(ip, _)| ip.clone())
            .collect();
        for ip in expired {
            self.bans.remove(&ip);
            self.strikes.remove(&ip);
            tracing::info!("✅ Ban expired for {}", ip);
        }
    }

    /// Number of currently active bans
    pub fn ban_count(&self) -> usize {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        self.bans.values().filter(|e| now < e.banned_until).count()
    }
}

// ─── Mempool (Fee-Rate Sorted) ──────────────────────────────────────

struct MempoolEntry {
    tx: Transaction,
    fee: u64,
    size: usize,
    /// Fee rate in base units per byte (fee / tx_size)
    fee_rate: f64,
}

pub struct Mempool {
    entries: HashMap<Hash256, MempoolEntry>,
    max_size: usize,
}

impl Mempool {
    pub fn new(max_size: usize) -> Self {
        Self { entries: HashMap::new(), max_size }
    }

    /// Add a pre-validated transaction with a known fee
    pub fn add_with_fee(&mut self, tx: Transaction, fee: u64) -> bool {
        let txid = crate::crypto::txid::txid_v1(&tx);
        if self.entries.contains_key(&txid) { return false; }
        if self.entries.len() >= self.max_size { return false; }
        let size = tx.size();
        let fee_rate = if size > 0 { fee as f64 / size as f64 } else { 0.0 };
        self.entries.insert(txid, MempoolEntry { tx, fee, size, fee_rate });
        true
    }

    /// Add without fee info (legacy, used for pre-validated txs)
    pub fn add(&mut self, tx: Transaction) -> bool {
        self.add_with_fee(tx, 0)
    }

    /// Validate against the chain and add with computed fee
    pub fn validate_and_add(&mut self, tx: Transaction, chain: &Chain) -> Result<Hash256, String> {
        let txid = crate::crypto::txid::txid_v1(&tx);
        if self.entries.contains_key(&txid) { return Err("duplicate transaction".into()); }
        if self.entries.len() >= self.max_size { return Err("mempool full".into()); }

        chain.validate_transaction_for_mempool(&tx).map_err(|e| format!("{}", e))?;

        // Calculate fee
        let mut input_sum: u64 = 0;
        for input in &tx.inputs {
            if let Some(utxo) = chain.utxo_set.get(&input.previous_output) {
                input_sum += utxo.output.amount;
            }
        }
        let fee = input_sum.saturating_sub(tx.total_output());
        self.add_with_fee(tx, fee);
        Ok(txid)
    }

    pub fn remove_confirmed(&mut self, block: &Block) {
        for tx in &block.transactions {
            if !tx.is_coinbase() {
                self.entries.remove(&tx.hash());
            }
        }
        // Also remove txs that spend now-consumed UTXOs (conflicting txs)
        let spent_outpoints: HashSet<OutPoint> = block.transactions.iter()
            .flat_map(|tx| tx.inputs.iter().map(|i| i.previous_output.clone()))
            .collect();
        self.entries.retain(|_, entry| {
            !entry.tx.inputs.iter().any(|i| spent_outpoints.contains(&i.previous_output))
        });
    }

    /// Get pending transactions sorted by fee rate (highest first)
    pub fn get_pending(&self) -> Vec<Transaction> {
        let mut entries: Vec<&MempoolEntry> = self.entries.values().collect();
        entries.sort_by(|a, b| b.fee_rate.partial_cmp(&a.fee_rate).unwrap_or(std::cmp::Ordering::Equal));
        entries.into_iter().map(|e| e.tx.clone()).collect()
    }

    /// Get pending with fee info (for RPC)
    pub fn get_pending_with_fees(&self) -> Vec<(Transaction, u64, f64)> {
        let mut entries: Vec<&MempoolEntry> = self.entries.values().collect();
        entries.sort_by(|a, b| b.fee_rate.partial_cmp(&a.fee_rate).unwrap_or(std::cmp::Ordering::Equal));
        entries.into_iter().map(|e| (e.tx.clone(), e.fee, e.fee_rate)).collect()
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

// ─── Shared Node State ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub address: String,
    pub listen_address: String,
    pub version: u32,
    pub best_height: u64,
    pub last_seen: u64,
    pub supports_v2: bool,
}

pub struct NodeState {
    pub chain: RwLock<Chain>,
    pub mempool: Mutex<Mempool>,
    pub peers: RwLock<HashMap<String, PeerInfo>>,
    pub known_addresses: RwLock<HashSet<String>>,
    pub scoreboard: Mutex<PeerScoreboard>,
    pub listen_port: u16,
    pub block_tx: broadcast::Sender<Block>,
    pub tx_tx: broadcast::Sender<Transaction>,
    /// Notifies the miner to restart with a new template when a block arrives
    pub new_block_notify: tokio::sync::Notify,
    /// Compact-block reconstruction state (Monero-like "fluffy blocks")
    pub pending_compacts: tokio::sync::Mutex<HashMap<Hash256, PendingCompact>>,
}

impl NodeState {
    pub fn new(listen_port: u16) -> Arc<Self> {
        let (block_tx, _) = broadcast::channel(256);
        let (tx_tx, _) = broadcast::channel(4096);
        Arc::new(Self {
            chain: RwLock::new(Chain::new()),
            mempool: Mutex::new(Mempool::new(10_000)),
            peers: RwLock::new(HashMap::new()),
            known_addresses: RwLock::new(HashSet::new()),
            scoreboard: Mutex::new(PeerScoreboard::new()),
            listen_port, block_tx, tx_tx,
            new_block_notify: tokio::sync::Notify::new(),
            pending_compacts: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn open(data_dir: &str, listen_port: u16) -> Arc<Self> {
        let (block_tx, _) = broadcast::channel(256);
        let (tx_tx, _) = broadcast::channel(4096);
        let chain = Chain::open(data_dir).unwrap_or_else(|e| {
            tracing::error!("Failed to open chain from {}: {}", data_dir, e);
            Chain::new()
        });
        Arc::new(Self {
            chain: RwLock::new(chain),
            mempool: Mutex::new(Mempool::new(10_000)),
            peers: RwLock::new(HashMap::new()),
            known_addresses: RwLock::new(HashSet::new()),
            scoreboard: Mutex::new(PeerScoreboard::new()),
            listen_port, block_tx, tx_tx,
            new_block_notify: tokio::sync::Notify::new(),
            pending_compacts: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
}

// ─── Connection Handler ─────────────────────────────────────────────

async fn handle_connection(mut stream: TcpStream, state: Arc<NodeState>, peer_addr: String, is_outbound: bool) {
    // Check ban before anything
    {
        let sb = state.scoreboard.lock().await;
        if sb.is_banned(&peer_addr) {
            tracing::debug!("🚫 Rejected banned peer {}", peer_addr);
            return;
        }
    }

    // TCP optimizations
    let _ = stream.set_nodelay(true);

    let direction = if is_outbound { "Outbound" } else { "Inbound" };
    tracing::info!("🔗 {} connection: {}", direction, peer_addr);

    let (our_height, our_hash, our_genesis) = {
        let chain = state.chain.read().await;
        let genesis = chain.block_at_height(0).map(|b| b.header.hash()).unwrap_or(NULL_HASH);
        (chain.height, chain.tip, genesis)
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    let version_msg = NetMessage::VersionV2 {
        version: PROTOCOL_VERSION, best_height: our_height, best_hash: our_hash,
        genesis_hash: our_genesis, timestamp: now, listen_port: state.listen_port,
    };
    if let Err(e) = write_message(&mut stream, &version_msg).await {
        tracing::error!("Failed to send version to {}: {}", peer_addr, e);
        return;
    }

    // Track if peer supports v2 protocol
    let peer_is_v2;

    let peer_height = match read_message(&mut stream).await {
        Ok(NetMessage::VersionV2 { version, best_height, genesis_hash, listen_port, .. }) => {
            peer_is_v2 = true;

            // Reject outdated protocol versions
            if version < MIN_PROTOCOL_VERSION {
                tracing::warn!("🚫 Rejecting peer {} — protocol v{} too old (minimum v{})", 
                    peer_addr, version, MIN_PROTOCOL_VERSION);
                return;
            }

            // Verify genesis match
            if genesis_hash != our_genesis {
                tracing::warn!(
                    "❌ Peer {} has different genesis! Theirs: {} Ours: {}",
                    peer_addr,
                    &hex::encode(genesis_hash)[..16],
                    &hex::encode(our_genesis)[..16]
                );

                // Wrong network. Never reset local chain based on peer state.
                return;
            }


            tracing::info!("  Peer {} v{} at height {} (genesis verified ✅)", peer_addr, version, best_height);
            {
                let peer_ip = peer_addr.split(':').next().unwrap_or("127.0.0.1");
                let listen_addr = format!("{}:{}", peer_ip, listen_port);
                let mut peers = state.peers.write().await;
                peers.insert(peer_addr.clone(), PeerInfo {
                    address: peer_addr.clone(), listen_address: listen_addr.clone(),
                    version, best_height, last_seen: now, supports_v2: true,
                });
                drop(peers);
                let mut known = state.known_addresses.write().await;
                known.insert(listen_addr);
            }
            let _ = write_message(&mut stream, &NetMessage::VersionAck).await;
            best_height
        }
        Ok(NetMessage::Version { version, listen_port, .. }) => {
            // Old nodes using legacy Version message are rejected
            tracing::warn!("🚫 Rejecting peer {} — legacy protocol v{}, must upgrade to v{}", 
                peer_addr, version, MIN_PROTOCOL_VERSION);
            return;
        }
        Ok(_) => {
            let mut sb = state.scoreboard.lock().await;
            sb.record_offense(&peer_addr, Offense::MalformedMessage);
            return;
        }
        Err(e) => { tracing::error!("Version read from {}: {}", peer_addr, e); return; }
    };

    // Re-read our height after potential reset
    let our_height = state.chain.read().await.height;

    match tokio::time::timeout(std::time::Duration::from_secs(5), read_message(&mut stream)).await {
        Ok(Ok(NetMessage::VersionAck)) => tracing::info!("  ✅ Handshake with {}", peer_addr),
        Ok(Ok(NetMessage::Version { .. })) | Ok(Ok(NetMessage::VersionV2 { .. })) => {
            let _ = write_message(&mut stream, &NetMessage::VersionAck).await;
            tracing::info!("  ✅ Handshake with {}", peer_addr);
        }
        _ => tracing::info!("  ✅ Handshake with {} (no ack)", peer_addr),
    }

    if peer_height > our_height {
        tracing::info!("📥 Peer {} ahead ({} vs {}), syncing (headers-first with locator)...",
            peer_addr, peer_height, our_height);

        // Always use locator-based sync — handles forks correctly
        let locator = {
            let chain = state.chain.read().await;
            build_locator(&chain, 32)
        };
        let _ = write_message(&mut stream, &NetMessage::GetHeadersFrom {
            locator,
            count: 2000,
        }).await;
    }

    let _ = write_message(&mut stream, &NetMessage::GetPeers).await;

    let mut block_rx = state.block_tx.subscribe();
    let mut tx_rx = state.tx_tx.subscribe();
    let mut peer_exchange = tokio::time::interval(std::time::Duration::from_secs(PEER_EXCHANGE_INTERVAL));
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(60));

    loop {
        tokio::select! {
            msg_result = tokio::time::timeout(
                std::time::Duration::from_secs(300), // 5 min read timeout
                read_message(&mut stream)
            ) => {
                match msg_result {
                    Ok(Ok(msg)) => {
                        match handle_message(&mut stream, &state, &peer_addr, msg).await {
                            Ok(()) => {}
                            Err(e) => {
                                tracing::error!("Error from {}: {}", peer_addr, e);
                                break;
                            }
                        }
                        // Check if peer got banned during message handling
                        let sb = state.scoreboard.lock().await;
                        if sb.is_banned(&peer_addr) {
                            tracing::info!("🚫 Disconnecting banned peer {}", peer_addr);
                            break;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::info!("🔌 Peer {} disconnected: {}", peer_addr, e);
                        break;
                    }
                    Err(_) => {
                        tracing::info!("🔌 Peer {} timed out (no messages for 5 min)", peer_addr);
                        break;
                    }
                }
            }
            block_result = block_rx.recv() => {
                if let Ok(block) = block_result {
                    if peer_is_v2 {
                        // Send compact block: full coinbase + hashes of remaining txs
                        let tx_hashes: Vec<Hash256> = block.transactions[1..].iter()
                            .map(|tx| tx.hash())
                            .collect();
                        let _ = write_message(&mut stream, &NetMessage::CompactBlock {
                            header: block.header.clone(),
                            short_txids: tx_hashes,
                            coinbase: block.transactions[0].clone(),
                        }).await;
                    } else {
                        let _ = write_message(&mut stream, &NetMessage::NewBlock(block)).await;
                    }
                }
            }
            tx_result = tx_rx.recv() => {
                if let Ok(tx) = tx_result {
                    let _ = write_message(&mut stream, &NetMessage::NewTransaction(tx)).await;
                }
            }
            _ = peer_exchange.tick() => {
                let _ = write_message(&mut stream, &NetMessage::GetPeers).await;
            }
            _ = keepalive.tick() => {
                let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                if write_message(&mut stream, &NetMessage::Ping(nonce)).await.is_err() {
                    tracing::info!("🔌 Peer {} unreachable (ping failed)", peer_addr);
                    break;
                }
                // Update last_seen
                let mut peers = state.peers.write().await;
                if let Some(peer) = peers.get_mut(&peer_addr) {
                    peer.last_seen = nonce;
                }
            }
        }
    }

    { state.peers.write().await.remove(&peer_addr); }
    tracing::info!("🔌 Cleaned up peer {}", peer_addr);
}

// ─── Message Handler ────────────────────────────────────────────────

async fn handle_message(
    stream: &mut TcpStream, state: &Arc<NodeState>, peer_addr: &str, msg: NetMessage,
) -> Result<(), String> {
    match msg {
        NetMessage::NewBlock(block) => {
            let height = block.header.height;
            let hash = block.header.hash();
            let mut chain = state.chain.write().await;
            match chain.add_block(block.clone()) {
                Ok(_) => {
                    drop(chain);
                    let mut mempool = state.mempool.lock().await;
                    mempool.remove_confirmed(&block);
                    drop(mempool);
                    let _ = state.block_tx.send(block);
                    // Tell miner to restart with new template
                    state.new_block_notify.notify_waiters();
                    tracing::info!("📦 Block #{} from {} ({})", height, peer_addr, &hex::encode(hash)[..16]);
                    let mut peers = state.peers.write().await;
                    if let Some(peer) = peers.get_mut(peer_addr) {
                        peer.best_height = peer.best_height.max(height);
                        peer.last_seen = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                    }
                }
                Err(crate::core::chain::BlockError::OrphanBlock) => {
                    let our_height = chain.height;
                    drop(chain);
                    tracing::info!("📥 Block #{} is orphan, locator-syncing from {} (we're at {})", height, peer_addr, our_height);
                    // Use locator to handle forks correctly — never assume linear chain
                    let locator = {
                        let chain = state.chain.read().await;
                        build_locator(&chain, 32)
                    };
                    write_message(stream, &NetMessage::GetHeadersFrom {
                        locator,
                        count: 2000,
                    }).await?;
                }
                Err(e) => {
                    tracing::warn!("❌ Block #{} from {} rejected: {}", height, peer_addr, e);
                    // OrphanBlock and DuplicateBlock are normal during propagation — no penalty
                    let is_harmless = matches!(e,
                        crate::core::chain::BlockError::DuplicateBlock |
                        crate::core::chain::BlockError::InvalidHeight |
                        crate::core::chain::BlockError::OrphanBlock
                    );
                    if !is_harmless {
                        let mut sb = state.scoreboard.lock().await;
                        sb.record_offense(peer_addr, Offense::InvalidBlock);
                    }
                }
            }
        }

        NetMessage::GetHeadersFrom { locator, count } => {
            let capped = count.min(2000);

            let chain = state.chain.read().await;

            // Find the newest locator hash we recognize (locator is newest->oldest)
            let mut start_height = 0u64;
            for h in &locator {
                if let Some(hdr) = chain.header(h) {
                    start_height = hdr.height.saturating_add(1);
                    break;
                }
            }

            let headers = chain.headers_in_range(start_height, capped);
            drop(chain);

            if !headers.is_empty() {
                write_message(stream, &NetMessage::Headers(headers)).await?;
            }
        }


        NetMessage::NewTransaction(tx) => {
            if tx.is_coinbase() {
                let mut sb = state.scoreboard.lock().await;
                sb.record_offense(peer_addr, Offense::InvalidTransaction);
                return Ok(());
            }
            let chain = state.chain.read().await;
            let mut mempool = state.mempool.lock().await;
            match mempool.validate_and_add(tx.clone(), &chain) {
                Ok(txid) => {
                    drop(mempool); drop(chain);
                    tracing::debug!("📝 Validated tx from {}: {}", peer_addr, hex::encode(txid));
                    let _ = state.tx_tx.send(tx);
                }
                Err(e) => {
                    tracing::debug!("Rejected tx from {}: {}", peer_addr, e);
                    let mut sb = state.scoreboard.lock().await;
                    sb.record_offense(peer_addr, Offense::InvalidTransaction);
                }
            }
        }

        NetMessage::GetBlocks { start_height, count } => {
            // Rate-limit: cap at 500 blocks, and limit how much data we send
            let capped_count = count.min(500);
            let chain = state.chain.read().await;
            let mut blocks = Vec::new();
            let end = (start_height + capped_count as u64).min(chain.height + 1);
            for h in start_height..end {
                if let Some(block) = chain.block_at_height(h) { blocks.push(block.clone()); }
            }
            let send_count = blocks.len();
            drop(chain);
            if send_count > 0 {
                tracing::info!("📤 Sending {} blocks to {} ({}→{})", send_count, peer_addr, start_height, start_height + send_count as u64 - 1);
                write_message(stream, &NetMessage::Blocks(blocks)).await?;
            }
        }

        NetMessage::Blocks(blocks) => {
            let count = blocks.len();
            let mut accepted = 0;
            let is_batch_sync = count > 10;

            // Process in chunks of 25, releasing the lock between chunks
            // so the miner can submit blocks
            let chunk_size = 25;
            for chunk in blocks.chunks(chunk_size) {
                let mut chain = state.chain.write().await;
                if is_batch_sync { chain.set_batch_mode(true); }
                for block in chunk {
                    match chain.add_block(block.clone()) {
                        Ok(_) => {
                            accepted += 1;
                        }
                        Err(e) => tracing::warn!("❌ Sync block #{} from {} rejected: {}", block.header.height, peer_addr, e),
                    }
                }
                if is_batch_sync {
                    chain.set_batch_mode(false);
                    chain.flush_batch();
                }
                drop(chain);
                // Yield to let miner and other tasks run
                tokio::task::yield_now().await;
            }

            // Remove confirmed txs from mempool outside chain lock
            if accepted > 0 {
                let mut mempool = state.mempool.lock().await;
                for block in &blocks {
                    mempool.remove_confirmed(block);
                }
                drop(mempool);
                // Tell miner to restart with updated chain tip
                state.new_block_notify.notify_waiters();
            }

            let our_height = {
                let chain = state.chain.read().await;
                chain.height
            };
            tracing::info!("📥 Synced {}/{} from {} (height: {})", accepted, count, peer_addr, our_height);

            // Update peer's advertised height based on blocks received
            if let Some(last_block) = blocks.last() {
                let mut peers = state.peers.write().await;
                if let Some(peer) = peers.get_mut(peer_addr) {
                    peer.best_height = peer.best_height.max(last_block.header.height);
                }
                drop(peers);
            }

            let peers = state.peers.read().await;
            if let Some(peer) = peers.get(peer_addr) {
                if peer.best_height > our_height {
                    drop(peers);
                    // Use locator for fork-safe continuation
                    let locator = {
                        let chain = state.chain.read().await;
                        build_locator(&chain, 32)
                    };
                    write_message(stream, &NetMessage::GetHeadersFrom {
                        locator, count: 2000,
                    }).await?;
                }
            }
        }

        NetMessage::GetBlock(hash) => {
            let chain = state.chain.read().await;
            let block = chain.header(&hash)
                .and_then(|h| chain.block_at_height(h.height))
                .cloned();
            drop(chain);
            if let Some(block) = block {
                write_message(stream, &NetMessage::NewBlock(block)).await?;
            }
        }

        NetMessage::GetPeers => {
            let peers = state.peers.read().await;
            let addrs: Vec<String> = peers.values().map(|p| p.listen_address.clone()).collect();
            drop(peers);
            write_message(stream, &NetMessage::Peers(addrs)).await?;
        }

        NetMessage::Peers(addrs) => {
            let our_addr = format!("127.0.0.1:{}", state.listen_port);
            let mut known = state.known_addresses.write().await;
            let connected: HashSet<String> = {
                let peers = state.peers.read().await;
                peers.values().map(|p| p.listen_address.clone()).collect()
            };
            let mut new_count = 0u32;
            for addr in addrs {
                if addr == our_addr || connected.contains(&addr) { continue; }
                if known.insert(addr) { new_count += 1; }
            }
            drop(known);
            if new_count > 0 {
                tracing::debug!("Discovered {} new peer addresses from {}", new_count, peer_addr);
            }
        }

        NetMessage::Ping(nonce) => {
            write_message(stream, &NetMessage::Pong(nonce)).await?;
        }

        NetMessage::Pong(_) => {
            let mut peers = state.peers.write().await;
            if let Some(peer) = peers.get_mut(peer_addr) {
                peer.last_seen = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            }
        }

        NetMessage::Version { .. } | NetMessage::VersionAck | NetMessage::VersionV2 { .. } => {}

        // ─── Headers-First Sync ───

        NetMessage::GetHeaders { start_height, count } => {
            let capped = count.min(2000); // Headers are small, can send more
            let chain = state.chain.read().await;
            let headers = chain.headers_in_range(start_height, capped);
            drop(chain);
            if !headers.is_empty() {
                tracing::info!("📤 Sending {} headers to {} ({}→{})",
                    headers.len(), peer_addr, start_height, start_height + headers.len() as u64 - 1);
                write_message(stream, &NetMessage::Headers(headers)).await?;
            }
        }

        NetMessage::Headers(headers) => {
            let count = headers.len();
            if count == 0 { return Ok(()); }

            let first_height = headers[0].height;
            let last_height = headers.last().map(|h| h.height).unwrap_or(0);

            // Validate the header chain (PoW check, parent linkage)
            let valid_hashes = {
                let chain = state.chain.read().await;
                chain.validate_header_chain(&headers)
            };

            if valid_hashes.is_empty() {
                tracing::debug!(
                    "All {} headers from {} rejected — locator resync",
                    count,
                    peer_addr
                );

                let locator = {
                    let chain = state.chain.read().await;
                    build_locator(&chain, 32)
                };

                write_message(stream, &NetMessage::GetHeadersFrom { locator, count: 2000 }).await?;
                return Ok(());
            }

            // Filter to hashes we don't have full blocks for
            let need_blocks: Vec<Hash256> = {
                let chain = state.chain.read().await;
                valid_hashes.iter()
                    .filter(|h| chain.block_by_hash(h).is_none())
                    .copied()
                    .collect()
            };

            tracing::info!("📥 Got {} headers from {} (heights {}→{}), need {} blocks",
                count, peer_addr, first_height, last_height, need_blocks.len());

            // Update peer's advertised height so sync continues correctly
            {
                let mut peers = state.peers.write().await;
                if let Some(peer) = peers.get_mut(peer_addr) {
                    peer.best_height = peer.best_height.max(last_height);
                }
            }

            // Request full block data for validated headers
            if !need_blocks.is_empty() {
                // Request in batches of 100
                for chunk in need_blocks.chunks(100) {
                    write_message(stream, &NetMessage::GetBlockData(chunk.to_vec())).await?;
                }
            }

            // Request more headers if peer has more
            let peers = state.peers.read().await;
            if let Some(peer) = peers.get(peer_addr) {
                if peer.best_height > last_height {
                    drop(peers);
                    write_message(stream, &NetMessage::GetHeaders {
                        start_height: last_height + 1, count: 2000,
                    }).await?;
                }
            }
        }

        NetMessage::GetBlockData(hashes) => {
            let capped = if hashes.len() > 100 { &hashes[..100] } else { &hashes };
            let chain = state.chain.read().await;
            let blocks = chain.blocks_by_hashes(capped);
            drop(chain);
            if !blocks.is_empty() {
                tracing::info!("📤 Sending {} block data to {}", blocks.len(), peer_addr);
                write_message(stream, &NetMessage::BlockData(blocks)).await?;
            }
        }

        NetMessage::BlockData(blocks) => {
            let count = blocks.len();
            let mut accepted = 0;
            let mut last_reject_reason = String::new();
            let chunk_size = 25;
            for chunk in blocks.chunks(chunk_size) {
                let mut chain = state.chain.write().await;
                chain.set_batch_mode(true);
                for block in chunk {
                    match chain.add_block(block.clone()) {
                        Ok(_) => accepted += 1,
                        Err(e) => {
                            last_reject_reason = format!("{}", e);
                            tracing::warn!("❌ BlockData #{} rejected from {}: {}", block.header.height, peer_addr, e);
                        }
                    }
                }
                chain.set_batch_mode(false);
                chain.flush_batch();
                drop(chain);
                tokio::task::yield_now().await;
            }
            if accepted > 0 {
                let mut mempool = state.mempool.lock().await;
                for block in &blocks {
                    mempool.remove_confirmed(block);
                }
                drop(mempool);
                state.new_block_notify.notify_waiters();
            }
            let our_height = state.chain.read().await.height;
            tracing::info!("📥 BlockData: accepted {}/{} from {} (height: {})", accepted, count, peer_addr, our_height);

            // Update peer's advertised height based on blocks received
            if let Some(last_block) = blocks.last() {
                let mut peers = state.peers.write().await;
                if let Some(peer) = peers.get_mut(peer_addr) {
                    peer.best_height = peer.best_height.max(last_block.header.height);
                }
                drop(peers);
            }

            // Continue syncing if peer has more blocks
            let peer_best = {
                let peers = state.peers.read().await;
                peers.get(peer_addr).map(|p| p.best_height)
            };
            if let Some(best_height) = peer_best {
                if best_height > our_height {
                    // If we accepted some blocks, keep going with headers-first
                    // If we accepted none, try locator resync to find fork point
                    if accepted > 0 {
                        let locator = {
                        let chain = state.chain.read().await;
                        build_locator(&chain, 32)
                    };

                    write_message(
                        stream,
                        &NetMessage::GetHeadersFrom { locator, count: 2000 },
                    ).await?;

                    } else {
                        tracing::info!("📥 BlockData all rejected ({}), locator resync from {}", last_reject_reason, peer_addr);
                        let locator = {
                            let chain = state.chain.read().await;
                            build_locator(&chain, 32)
                        };
                        write_message(stream, &NetMessage::GetHeadersFrom {
                            locator, count: 2000,
                        }).await?;
                    }
                }
            }
        }

        // ─── Compact Block Relay ───

        NetMessage::CompactBlock { header, short_txids, coinbase } => {
            // Monero-like "fluffy block": try reconstruct from mempool, request only missing txs.
            let block_hash = header.hash();

            // Fast-path: if we already have this block, ignore.
            {
                let chain = state.chain.read().await;
                if chain.block_by_hash(&block_hash).is_some() {
                    tracing::debug!("📦 Compact block already known: {}", &hex::encode(block_hash)[..16]);
                    return Ok(());
                }
            }

            // Verify PoW BEFORE any reconstruction — prevents resource exhaustion
            if leading_zero_bits(&block_hash) < header.difficulty_target {
                tracing::warn!("❌ Compact block from {} has invalid PoW, banning", peer_addr);
                let mut sb = state.scoreboard.lock().await;
                sb.record_offense(peer_addr, Offense::InvalidBlock);
                sb.record_offense(peer_addr, Offense::InvalidBlock); // double strike for bad PoW
                return Ok(());
            }

            // Build reconstruction vector: [coinbase, ...]
            let mut txs: Vec<Option<Transaction>> = Vec::with_capacity(1 + short_txids.len());
            txs.push(Some(coinbase.clone()));

            let mut missing: std::collections::HashSet<Hash256> = std::collections::HashSet::new();
            let mut index_map: HashMap<Hash256, usize> = HashMap::new();

            {
                let mempool = state.mempool.lock().await;
                let pending = mempool.get_pending();
                let pending_map: HashMap<Hash256, &Transaction> = pending.iter()
                    .map(|tx| (crate::crypto::txid::txid_v1(tx), tx))
                    .collect();

                for txid in &short_txids {
                    let idx = txs.len();
                    if let Some(tx) = pending_map.get(txid) {
                        txs.push(Some((*tx).clone()));
                    } else {
                        txs.push(None);
                        missing.insert(*txid);
                    }
                    index_map.insert(*txid, idx);
                }
            }

            if missing.is_empty() {
                // Fully reconstructed
                let full_txs: Vec<Transaction> = txs.into_iter().map(|t| t.unwrap()).collect();
                let block = Block { header, transactions: full_txs };

                let mut chain = state.chain.write().await;
                match chain.add_block(block.clone()) {
                    Ok(_) => {
                        drop(chain);
                        state.mempool.lock().await.remove_confirmed(&block);
                        let _ = state.block_tx.send(block);
                        state.new_block_notify.notify_waiters();
                        tracing::info!("📦 Compact block from {} ({})", peer_addr, &hex::encode(block_hash)[..16]);
                    }
                    Err(crate::core::chain::BlockError::OrphanBlock) => {
                        let our_height = chain.height;
                        drop(chain);
                        tracing::info!("📥 Compact block is orphan, locator-syncing from {} (we're at {})", peer_addr, our_height);
                        let locator = {
                            let chain = state.chain.read().await;
                            build_locator(&chain, 32)
                        };
                        write_message(stream, &NetMessage::GetHeadersFrom {
                            locator,
                            count: 2000,
                        }).await?;
                    }
                    Err(e) => {
                        tracing::warn!("❌ Compact block from {} rejected: {:?}", peer_addr, e);
                    }
                }
                return Ok(());
            }

            // Store pending and request missing txs
            {
                let mut pending = state.pending_compacts.lock().await;
                // Cap at 50 to prevent memory exhaustion
                if pending.len() >= 50 {
                    if let Some(oldest_hash) = pending.iter()
                        .min_by_key(|(_, pc)| pc.created_at)
                        .map(|(h, _)| *h) {
                        tracing::debug!("🗑️ Evicting oldest pending compact to make room");
                        pending.remove(&oldest_hash);
                    }
                }
                pending.insert(block_hash, PendingCompact {
                    header,
                    txs,
                    index_map,
                    missing: missing.clone(),
                    created_at: std::time::Instant::now(),
                });
            }

            let missing_list: Vec<Hash256> = missing.into_iter().collect();
            write_message(stream, &NetMessage::GetTransactions(missing_list)).await?;


        }

        NetMessage::GetTransactions(hashes) => {
            let mempool = state.mempool.lock().await;
            let pending = mempool.get_pending();
            drop(mempool);
            let pending_map: HashMap<Hash256, Transaction> = pending.into_iter()
                .map(|tx| (crate::crypto::txid::txid_v1(&tx), tx))
                .collect();

            let found: Vec<Transaction> = hashes.iter()
                .filter_map(|h| pending_map.get(h).cloned())
                .collect();
            if !found.is_empty() {
                write_message(stream, &NetMessage::TransactionBatch(found)).await?;
            }
        }

        NetMessage::TransactionBatch(txs) => {
            // Add to mempool and try satisfy any pending compact blocks.
            for tx in txs {
                let txid = crate::crypto::txid::txid_v1(&tx);

                // Add to mempool
                {
                    let chain = state.chain.read().await;
                    let mut mempool = state.mempool.lock().await;
                    let _ = mempool.validate_and_add(tx.clone(), &chain);
                }

                tracing::debug!("📦 Received tx {}...", &hex::encode(txid)[..16]);

                // Feed into pending compact blocks
                let mut completed: Vec<Hash256> = Vec::new();
                {
                    let mut pending = state.pending_compacts.lock().await;
                    for (block_hash, pc) in pending.iter_mut() {
                        if pc.missing.remove(&txid) {
                            if let Some(&idx) = pc.index_map.get(&txid) {
                                pc.txs[idx] = Some(tx.clone());
                            }
                        }
                        if pc.missing.is_empty() {
                            completed.push(*block_hash);
                        }
                    }
                }

                // Attempt to finalize completed compact blocks
                for bh in completed {
                    let pc = {
                        let mut pending = state.pending_compacts.lock().await;
                        pending.remove(&bh)
                    };
                    if let Some(pc) = pc {
                        if pc.txs.iter().any(|t| t.is_none()) {
                            continue;
                        }
                        let full_txs: Vec<Transaction> = pc.txs.into_iter().map(|t| t.unwrap()).collect();
                        let block = Block { header: pc.header, transactions: full_txs };

                        let mut chain = state.chain.write().await;
                        match chain.add_block(block.clone()) {
                            Ok(_) => {
                                drop(chain);
                                state.mempool.lock().await.remove_confirmed(&block);
                                let _ = state.block_tx.send(block);
                                state.new_block_notify.notify_waiters();
                                tracing::info!("✅ Reconstructed block {} from compact+missing txs", &hex::encode(bh)[..16]);
                            }
                            Err(e) => {
                                tracing::warn!("❌ Reconstructed block rejected: {:?}", e);
                            }
                        }
                    }
                }
            }

        }
    }
    Ok(())
}

// ─── Public API ─────────────────────────────────────────────────────

pub async fn start_node(
    state: Arc<NodeState>, seed_peers: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listen_addr = format!("0.0.0.0:{}", state.listen_port);
    let listener = TcpListener::bind(&listen_addr).await?;
    tracing::info!("🌐 Listening on {}", listen_addr);

    let mut all_seeds: Vec<String> = seed_peers;
    for seed in seed_nodes() {
        let s = seed.to_string();
        if !all_seeds.contains(&s) { all_seeds.push(s); }
    }

    for addr in &all_seeds {
        let state = state.clone();
        let addr = addr.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            connect_to_peer(state, &addr).await;
        });
    }

    // Maintenance task
    {
        let state = state.clone();
        let seeds = all_seeds.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;

                // Clean up expired bans
                { state.scoreboard.lock().await.cleanup(); }

                // Expire stale pending compact blocks (>30s old)
                {
                    let mut pending = state.pending_compacts.lock().await;
                    let now = std::time::Instant::now();
                    let before = pending.len();
                    pending.retain(|hash, pc| {
                        let age = now.duration_since(pc.created_at).as_secs();
                        if age > 30 {
                            tracing::debug!("🗑️ Expiring stale compact block {}", &hex::encode(hash)[..16]);
                            false
                        } else {
                            true
                        }
                    });
                    let expired = before - pending.len();
                    if expired > 0 {
                        tracing::debug!("🗑️ Expired {} stale compact blocks", expired);
                    }
                }

                let peer_count = state.peers.read().await.len();

                // Retry seeds if no peers (more aggressive — every 30s instead of 60s)
                if peer_count == 0 && !seeds.is_empty() {
                    tracing::info!("🔄 No peers, retrying seeds...");
                    for seed in &seeds {
                        let state = state.clone();
                        let addr = seed.clone();
                        tokio::spawn(async move { connect_to_peer(state, &addr).await; });
                    }
                }

                // Try discovered peers if below target
                if peer_count > 0 && peer_count < MAX_OUTBOUND_PEERS {
                    let known = state.known_addresses.read().await;
                    let connected: HashSet<String> = {
                        let peers = state.peers.read().await;
                        peers.values().map(|p| p.listen_address.clone()).collect()
                    };
                    let our_addr = format!("127.0.0.1:{}", state.listen_port);
                    let sb = state.scoreboard.lock().await;

                    let candidates: Vec<String> = known.iter()
                        .filter(|a| *a != &our_addr && !connected.contains(*a) && !sb.is_banned(a))
                        .take(3)
                        .cloned()
                        .collect();
                    drop(sb);
                    drop(known);

                    for addr in candidates {
                        let state = state.clone();
                        tokio::spawn(async move { connect_to_peer(state, &addr).await; });
                    }
                }

                // Prune stale peers (no messages for 5 min)
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                state.peers.write().await.retain(|addr, info| {
                    let stale = now - info.last_seen > 300;
                    if stale { tracing::info!("🔌 Pruning stale peer {}", addr); }
                    !stale
                });

                // Save anchor connections
                let peers = state.peers.read().await;
                let mut anchor_candidates: Vec<String> = peers.values()
                    .map(|p| p.listen_address.clone())
                    .collect();
                drop(peers);
                anchor_candidates.truncate(MAX_ANCHORS);
                if !anchor_candidates.is_empty() {
                    save_anchors(data_dir(), &anchor_candidates);
                }
            }
        });
    }

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let peer_addr = addr.to_string();
                // Check ban
                {
                    let sb = state.scoreboard.lock().await;
                    if sb.is_banned(&peer_addr) {
                        tracing::debug!("🚫 Rejected banned inbound {}", peer_addr);
                        continue;
                    }
                }
                let peer_count = state.peers.read().await.len();
                if peer_count >= MAX_PEERS {
                    tracing::debug!("Max peers, rejecting {}", addr);
                    continue;
                }
                let state = state.clone();
                tokio::spawn(async move {
                    handle_connection(stream, state, peer_addr, false).await;
                });
            }
            Err(e) => tracing::error!("Accept error: {}", e),
        }
    }
}

pub async fn connect_to_peer(state: Arc<NodeState>, addr: &str) {
    {
        let sb = state.scoreboard.lock().await;
        if sb.is_banned(addr) { return; }
    }
    {
        let peers = state.peers.read().await;
        if peers.values().any(|p| p.listen_address == addr || p.address == addr) { return; }
    }
    tracing::info!("🔗 Connecting to {}...", addr);
    // 10 second connection timeout to prevent hanging on dead peers
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        TcpStream::connect(addr)
    ).await {
        Ok(Ok(stream)) => handle_connection(stream, state, addr.to_string(), true).await,
        Ok(Err(e)) => tracing::debug!("Failed to connect to {}: {}", addr, e),
        Err(_) => tracing::debug!("Connection to {} timed out", addr),
    }
}

pub async fn broadcast_block(state: &Arc<NodeState>, block: Block) {
    let block_hash = block.header.hash();
    let mut chain = state.chain.write().await;
    match chain.add_block(block.clone()) {
        Ok(_) => {
            let height = chain.height;
            drop(chain);
            state.mempool.lock().await.remove_confirmed(&block);
            let _ = state.block_tx.send(block);
            state.new_block_notify.notify_waiters();
            tracing::info!("📡 Broadcast block #{} ({})", height, hex::encode(block_hash));
        }
        Err(crate::core::chain::BlockError::DuplicateBlock) => {
            tracing::debug!("Mined block already known (race with peer), discarding");
        }
        Err(crate::core::chain::BlockError::OrphanBlock) => {
            tracing::info!("⛏️  Mined block stale (chain moved while mining), discarding");
        }
        Err(e) => tracing::error!("Failed to add own block: {}", e),
    }
}

pub async fn get_node_info(state: &Arc<NodeState>) -> (u64, Hash256, usize, usize) {
    let chain = state.chain.read().await;
    let h = chain.height; let t = chain.tip; let u = chain.utxo_set.len();
    drop(chain);
    let p = state.peers.read().await.len();
    (h, t, u, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_encode_decode() {
        let _ = std::panic::catch_unwind(|| init_network(false));
        let msg = NetMessage::Version {
            version: 1, best_height: 42, best_hash: [0xABu8; 32],
            timestamp: 1234567890, listen_port: 9333,
        };
        let encoded = encode_message(&msg);
        assert_eq!(&encoded[0..4], &magic_bytes());
        let len = u32::from_le_bytes(encoded[4..8].try_into().unwrap()) as usize;
        let decoded: NetMessage = bincode::deserialize(&encoded[8..8 + len]).unwrap();
        match decoded {
            NetMessage::Version { version, best_height, listen_port, .. } => {
                assert_eq!(version, 1); assert_eq!(best_height, 42); assert_eq!(listen_port, 9333);
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_ban_system() {
        let mut sb = PeerScoreboard::new();
        assert!(!sb.is_banned("1.2.3.4:9333"));
        // 5 strikes needed
        sb.record_offense("1.2.3.4:9333", Offense::InvalidTransaction); // 1
        assert!(!sb.is_banned("1.2.3.4:9333"));
        sb.record_offense("1.2.3.4:9333", Offense::InvalidTransaction); // 2
        sb.record_offense("1.2.3.4:9333", Offense::InvalidBlock);       // 4
        assert!(!sb.is_banned("1.2.3.4:9333"));
        sb.record_offense("1.2.3.4:9333", Offense::InvalidTransaction); // 5 -> banned
        assert!(sb.is_banned("1.2.3.4:9333"));
        // Different port same IP also banned
        assert!(sb.is_banned("1.2.3.4:1234"));
    }

    #[test]
    fn test_mempool_fee_sorting() {
        let mut mp = Mempool::new(100);
        let tx1 = Transaction { version: 1, inputs: vec![], outputs: vec![TxOutput { amount: 100, pubkey_hash: [0; 32], script_pubkey: vec![] }], lock_time: 0 };
        let tx2 = Transaction { version: 1, inputs: vec![], outputs: vec![TxOutput { amount: 200, pubkey_hash: [1; 32], script_pubkey: vec![] }], lock_time: 0 };
        let tx3 = Transaction { version: 1, inputs: vec![], outputs: vec![TxOutput { amount: 300, pubkey_hash: [2; 32], script_pubkey: vec![] }], lock_time: 0 };
        mp.add_with_fee(tx1.clone(), 100);  // low fee
        mp.add_with_fee(tx2.clone(), 5000); // high fee
        mp.add_with_fee(tx3.clone(), 1000); // medium fee
        let pending = mp.get_pending();
        assert_eq!(pending.len(), 3);
        // Should be sorted: tx2 (highest fee rate) first
        assert_eq!(pending[0].hash(), tx2.hash());
    }

    #[tokio::test]
    async fn test_node_state() {
        let state = NodeState::new(9333);
        assert_eq!(state.chain.read().await.height, 0);
    }
}