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
}

// ─── Wire Protocol ───────────────────────────────────────────────────

const HEADER_SIZE: usize = 8;
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

pub fn encode_message(msg: &NetMessage) -> Vec<u8> {
    let payload = bincode::serialize(msg).expect("serialization failed");
    let mut data = Vec::with_capacity(HEADER_SIZE + payload.len());
    data.extend_from_slice(&TESTNET_MAGIC);
    data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    data.extend_from_slice(&payload);
    data
}

async fn read_message(stream: &mut TcpStream) -> Result<NetMessage, String> {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.map_err(|e| format!("read header: {}", e))?;
    if header[0..4] != TESTNET_MAGIC { return Err("invalid magic bytes".into()); }
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
        let txid = tx.hash();
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
        let txid = tx.hash();
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
}

impl NodeState {
    pub fn new(listen_port: u16) -> Arc<Self> {
        let (block_tx, _) = broadcast::channel(100);
        let (tx_tx, _) = broadcast::channel(1000);
        Arc::new(Self {
            chain: RwLock::new(Chain::new()),
            mempool: Mutex::new(Mempool::new(10_000)),
            peers: RwLock::new(HashMap::new()),
            known_addresses: RwLock::new(HashSet::new()),
            scoreboard: Mutex::new(PeerScoreboard::new()),
            listen_port, block_tx, tx_tx,
        })
    }

    pub fn open(data_dir: &str, listen_port: u16) -> Arc<Self> {
        let (block_tx, _) = broadcast::channel(100);
        let (tx_tx, _) = broadcast::channel(1000);
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

    let direction = if is_outbound { "Outbound" } else { "Inbound" };
    tracing::info!("🔗 {} connection: {}", direction, peer_addr);

    let (our_height, our_hash) = {
        let chain = state.chain.read().await;
        (chain.height, chain.tip)
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    let version_msg = NetMessage::Version {
        version: PROTOCOL_VERSION, best_height: our_height, best_hash: our_hash,
        timestamp: now, listen_port: state.listen_port,
    };
    if let Err(e) = write_message(&mut stream, &version_msg).await {
        tracing::error!("Failed to send version to {}: {}", peer_addr, e);
        return;
    }

    let peer_height = match read_message(&mut stream).await {
        Ok(NetMessage::Version { version, best_height, listen_port, .. }) => {
            tracing::info!("  Peer {} v{} at height {}", peer_addr, version, best_height);
            {
                let peer_ip = peer_addr.split(':').next().unwrap_or("127.0.0.1");
                let listen_addr = format!("{}:{}", peer_ip, listen_port);
                let mut peers = state.peers.write().await;
                peers.insert(peer_addr.clone(), PeerInfo {
                    address: peer_addr.clone(), listen_address: listen_addr.clone(),
                    version, best_height, last_seen: now,
                });
                drop(peers);
                let mut known = state.known_addresses.write().await;
                known.insert(listen_addr);
            }
            let _ = write_message(&mut stream, &NetMessage::VersionAck).await;
            best_height
        }
        Ok(_) => {
            let mut sb = state.scoreboard.lock().await;
            sb.record_offense(&peer_addr, Offense::MalformedMessage);
            return;
        }
        Err(e) => { tracing::error!("Version read from {}: {}", peer_addr, e); return; }
    };

    match tokio::time::timeout(std::time::Duration::from_secs(5), read_message(&mut stream)).await {
        Ok(Ok(NetMessage::VersionAck)) => tracing::info!("  ✅ Handshake with {}", peer_addr),
        Ok(Ok(NetMessage::Version { .. })) => {
            let _ = write_message(&mut stream, &NetMessage::VersionAck).await;
            tracing::info!("  ✅ Handshake with {}", peer_addr);
        }
        _ => tracing::info!("  ✅ Handshake with {} (no ack)", peer_addr),
    }

    if peer_height > our_height {
        tracing::info!("📥 Peer {} ahead ({} vs {}), syncing...", peer_addr, peer_height, our_height);
        let _ = write_message(&mut stream, &NetMessage::GetBlocks {
            start_height: our_height + 1,
            count: (peer_height - our_height).min(500) as u32,
        }).await;
    }

    let _ = write_message(&mut stream, &NetMessage::GetPeers).await;

    let mut block_rx = state.block_tx.subscribe();
    let mut tx_rx = state.tx_tx.subscribe();
    let mut peer_exchange = tokio::time::interval(std::time::Duration::from_secs(PEER_EXCHANGE_INTERVAL));

    loop {
        tokio::select! {
            msg_result = read_message(&mut stream) => {
                match msg_result {
                    Ok(msg) => {
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
                    Err(e) => {
                        tracing::info!("🔌 Peer {} disconnected: {}", peer_addr, e);
                        break;
                    }
                }
            }
            block_result = block_rx.recv() => {
                if let Ok(block) = block_result {
                    let _ = write_message(&mut stream, &NetMessage::NewBlock(block)).await;
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
                    tracing::info!("📦 Block #{} from {} ({})", height, peer_addr, &hex::encode(hash)[..16]);
                    let mut peers = state.peers.write().await;
                    if let Some(peer) = peers.get_mut(peer_addr) {
                        peer.best_height = peer.best_height.max(height);
                        peer.last_seen = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                    }
                }
                Err(e) => {
                    tracing::debug!("Block #{} from {} rejected: {}", height, peer_addr, e);
                    // Only penalize for truly invalid blocks, not duplicates or stale blocks
                    let is_harmless = matches!(e,
                        crate::core::chain::BlockError::DuplicateBlock |
                        crate::core::chain::BlockError::OrphanBlock |
                        crate::core::chain::BlockError::InvalidHeight
                    );
                    if !is_harmless {
                        let mut sb = state.scoreboard.lock().await;
                        sb.record_offense(peer_addr, Offense::InvalidBlock);
                    }
                }
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
            let chain = state.chain.read().await;
            let mut blocks = Vec::new();
            let end = (start_height + count as u64).min(chain.height + 1);
            for h in start_height..end {
                if let Some(block) = chain.block_at_height(h) { blocks.push(block.clone()); }
            }
            tracing::info!("📤 Sending {} blocks to {}", blocks.len(), peer_addr);
            drop(chain);
            write_message(stream, &NetMessage::Blocks(blocks)).await?;
        }

        NetMessage::Blocks(blocks) => {
            let count = blocks.len();
            let mut accepted = 0;
            let is_batch_sync = count > 10;

            {
                let mut chain = state.chain.write().await;
                // Defer disk writes during bulk sync
                if is_batch_sync { chain.set_batch_mode(true); }
                for block in &blocks {
                    match chain.add_block(block.clone()) {
                        Ok(_) => {
                            accepted += 1;
                        }
                        Err(e) => tracing::debug!("Sync block rejected: {}", e),
                    }
                }
                if is_batch_sync {
                    chain.set_batch_mode(false);
                    chain.flush_batch();
                }
            }

            // Remove confirmed txs from mempool outside chain lock
            if accepted > 0 {
                let mut mempool = state.mempool.lock().await;
                for block in &blocks {
                    mempool.remove_confirmed(block);
                }
            }

            let our_height = {
                let chain = state.chain.read().await;
                chain.height
            };
            tracing::info!("📥 Synced {}/{} from {} (height: {})", accepted, count, peer_addr, our_height);

            let peers = state.peers.read().await;
            if let Some(peer) = peers.get(peer_addr) {
                if peer.best_height > our_height {
                    drop(peers);
                    write_message(stream, &NetMessage::GetBlocks {
                        start_height: our_height + 1, count: 500,
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

        NetMessage::Version { .. } | NetMessage::VersionAck => {}
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
    for seed in SEED_NODES {
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
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;

                // Clean up expired bans
                { state.scoreboard.lock().await.cleanup(); }

                let peer_count = state.peers.read().await.len();

                // Retry seeds if no peers
                if peer_count == 0 && !seeds.is_empty() {
                    tracing::info!("🔄 No peers, retrying seeds...");
                    for seed in &seeds {
                        let state = state.clone();
                        let addr = seed.clone();
                        tokio::spawn(async move { connect_to_peer(state, &addr).await; });
                    }
                }

                // Try discovered peers if below target
                if peer_count < MAX_OUTBOUND_PEERS {
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

                // Prune stale peers
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                state.peers.write().await.retain(|addr, info| {
                    let stale = now - info.last_seen > 300;
                    if stale { tracing::info!("🔌 Pruning stale peer {}", addr); }
                    !stale
                });
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
    match TcpStream::connect(addr).await {
        Ok(stream) => handle_connection(stream, state, addr.to_string(), true).await,
        Err(e) => tracing::debug!("Failed to connect to {}: {}", addr, e),
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
            tracing::info!("📡 Broadcast block #{} ({})", height, hex::encode(block_hash));
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
        let msg = NetMessage::Version {
            version: 1, best_height: 42, best_hash: [0xABu8; 32],
            timestamp: 1234567890, listen_port: 9333,
        };
        let encoded = encode_message(&msg);
        assert_eq!(&encoded[0..4], &TESTNET_MAGIC);
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
        let tx1 = Transaction { version: 1, inputs: vec![], outputs: vec![TxOutput { amount: 100, pubkey_hash: [0; 32] }], lock_time: 0 };
        let tx2 = Transaction { version: 1, inputs: vec![], outputs: vec![TxOutput { amount: 200, pubkey_hash: [1; 32] }], lock_time: 0 };
        let tx3 = Transaction { version: 1, inputs: vec![], outputs: vec![TxOutput { amount: 300, pubkey_hash: [2; 32] }], lock_time: 0 };
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
