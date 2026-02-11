use std::collections::HashMap;
use std::path::Path;
use crate::core::types::*;
use crate::core::params::*;
use crate::storage::Storage;

/// Represents an unspent transaction output in the UTXO set
#[derive(Debug, Clone)]
pub struct UtxoEntry {
    pub output: TxOutput,
    pub height: u64,
    pub is_coinbase: bool,
}

/// The UTXO set - tracks all unspent outputs (in-memory)
#[derive(Debug, Clone)]
pub struct UtxoSet {
    utxos: HashMap<OutPoint, UtxoEntry>,
}

impl UtxoSet {
    pub fn new() -> Self { Self { utxos: HashMap::new() } }
    pub fn add(&mut self, outpoint: OutPoint, entry: UtxoEntry) { self.utxos.insert(outpoint, entry); }
    pub fn spend(&mut self, outpoint: &OutPoint) -> Option<UtxoEntry> { self.utxos.remove(outpoint) }
    pub fn contains(&self, outpoint: &OutPoint) -> bool { self.utxos.contains_key(outpoint) }
    pub fn get(&self, outpoint: &OutPoint) -> Option<&UtxoEntry> { self.utxos.get(outpoint) }
    pub fn balance_of(&self, pubkey_hash: &Hash256) -> u64 {
        self.utxos.values().filter(|e| &e.output.pubkey_hash == pubkey_hash).map(|e| e.output.amount).sum()
    }
    pub fn utxos_for(&self, pubkey_hash: &Hash256) -> Vec<(OutPoint, &UtxoEntry)> {
        self.utxos.iter().filter(|(_, e)| &e.output.pubkey_hash == pubkey_hash).map(|(op, e)| (op.clone(), e)).collect()
    }
    pub fn len(&self) -> usize { self.utxos.len() }
    pub fn is_empty(&self) -> bool { self.utxos.is_empty() }
    pub fn iter(&self) -> impl Iterator<Item = (&OutPoint, &UtxoEntry)> { self.utxos.iter() }
}

// ─── LWMA Difficulty ────────────────────────────────────────────────

const DIFFICULTY_WINDOW: usize = 60;
const MIN_DIFFICULTY: u32 = 4;
const MAX_DIFFICULTY: u32 = 200;
const MAX_ADJUSTMENT_PER_BLOCK: f64 = 0.5;

pub fn calculate_next_difficulty_fractional(current_frac: f64, timestamps: &[u64]) -> f64 {
    let n = timestamps.len();
    if n < 2 { return current_frac; }
    let window = n.min(DIFFICULTY_WINDOW);
    let start = n - window;
    let mut weighted_sum: f64 = 0.0;
    let mut weight_total: f64 = 0.0;
    for i in 1..window {
        let solve_time = timestamps[start + i].saturating_sub(timestamps[start + i - 1]);
        let clamped = (solve_time as f64).clamp(1.0, TARGET_BLOCK_TIME as f64 * 6.0);
        let weight = i as f64;
        weighted_sum += clamped * weight;
        weight_total += weight;
    }
    if weight_total == 0.0 { return current_frac; }
    let avg = weighted_sum / weight_total;
    let ratio = avg / TARGET_BLOCK_TIME as f64;
    let raw_adj = -(ratio.ln() / 2.0_f64.ln());
    let warmup = ((window - 1) as f64 / DIFFICULTY_WINDOW as f64).min(1.0);
    let max_adj = MAX_ADJUSTMENT_PER_BLOCK * warmup;
    let adj = raw_adj.clamp(-max_adj, max_adj);
    (current_frac + adj).clamp(MIN_DIFFICULTY as f64, MAX_DIFFICULTY as f64)
}

pub fn fractional_to_integer_difficulty(frac: f64) -> u32 {
    (frac.round() as i32).clamp(MIN_DIFFICULTY as i32, MAX_DIFFICULTY as i32) as u32
}

pub fn calculate_next_difficulty(current: u32, timestamps: &[u64]) -> u32 {
    fractional_to_integer_difficulty(calculate_next_difficulty_fractional(current as f64, timestamps))
}

// ─── Cumulative Work ────────────────────────────────────────────────

fn block_work(difficulty: u32) -> f64 {
    2.0_f64.powi(difficulty as i32)
}

// ─── Chain ──────────────────────────────────────────────────────────

pub struct Chain {
    /// All known block headers, indexed by hash
    headers: HashMap<Hash256, BlockHeader>,
    /// All known full blocks
    blocks: HashMap<Hash256, Block>,
    /// Height index for the ACTIVE chain only
    height_index: HashMap<u64, Hash256>,
    /// Cumulative work for each block hash
    cumulative_work: HashMap<Hash256, f64>,
    /// Parent -> children mapping (for finding forks)
    children: HashMap<Hash256, Vec<Hash256>>,
    /// UTXO set for the active chain
    pub utxo_set: UtxoSet,
    /// Current best chain tip
    pub tip: Hash256,
    /// Current best chain height
    pub height: u64,
    /// Recent timestamps on the active chain (for LWMA)
    recent_timestamps: Vec<u64>,
    fractional_difficulty: f64,
    storage: Option<Storage>,
    /// When true, skip per-block disk writes (flush at end of batch)
    batch_mode: bool,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("height", &self.height)
            .field("tip", &hex::encode(self.tip))
            .field("utxos", &self.utxo_set.len())
            .field("known_blocks", &self.blocks.len())
            .finish()
    }
}

impl Chain {
    /// Create a new in-memory chain
    pub fn new() -> Self {
        let genesis = Self::create_genesis_block();
        let genesis_hash = genesis.header.hash();
        let work = block_work(genesis.header.difficulty_target);

        let mut chain = Chain {
            headers: HashMap::new(),
            blocks: HashMap::new(),
            height_index: HashMap::new(),
            cumulative_work: HashMap::new(),
            children: HashMap::new(),
            utxo_set: UtxoSet::new(),
            tip: genesis_hash,
            height: 0,
            recent_timestamps: vec![genesis.header.timestamp],
            fractional_difficulty: INITIAL_DIFFICULTY as f64,
            storage: None,
            batch_mode: false,
        };

        chain.apply_block_utxos(&genesis);
        chain.headers.insert(genesis_hash, genesis.header.clone());
        chain.height_index.insert(0, genesis_hash);
        chain.cumulative_work.insert(genesis_hash, work);
        chain.blocks.insert(genesis_hash, genesis);
        chain
    }

    /// Open with persistent storage
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let storage = Storage::open(path).map_err(|e| e.to_string())?;
        if storage.has_chain_data() {
            Self::load_from_storage(storage)
        } else {
            let mut chain = Self::new();
            chain.persist_genesis(&storage)?;
            chain.storage = Some(storage);
            Ok(chain)
        }
    }

    /// Reset chain to genesis, clearing all data but keeping storage backend.
    pub fn reset(&mut self) {
        let had_storage = self.storage.take();
        let genesis = Self::create_genesis_block();
        let genesis_hash = genesis.header.hash();
        let work = block_work(genesis.header.difficulty_target);

        self.headers.clear();
        self.blocks.clear();
        self.height_index.clear();
        self.cumulative_work.clear();
        self.children.clear();
        self.utxo_set = UtxoSet::new();
        self.tip = genesis_hash;
        self.height = 0;
        self.recent_timestamps = vec![genesis.header.timestamp];
        self.fractional_difficulty = INITIAL_DIFFICULTY as f64;
        self.batch_mode = false;

        self.apply_block_utxos(&genesis);
        self.headers.insert(genesis_hash, genesis.header.clone());
        self.height_index.insert(0, genesis_hash);
        self.cumulative_work.insert(genesis_hash, work);
        self.blocks.insert(genesis_hash, genesis);

        // Re-attach storage and persist fresh genesis
        if let Some(storage) = had_storage {
            let _ = storage.clear_all();
            let _ = self.persist_genesis(&storage);
            self.storage = Some(storage);
        }
    }

    fn load_from_storage(storage: Storage) -> Result<Self, String> {
        let tip = storage.get_tip().map_err(|e| e.to_string())?.ok_or("no tip")?;
        let height = storage.get_height().map_err(|e| e.to_string())?.ok_or("no height")?;
        let timestamps = storage.get_timestamps().map_err(|e| e.to_string())?
            .unwrap_or_else(|| vec![genesis_timestamp()]);
        let fractional_difficulty = storage.get_fractional_difficulty()
            .map_err(|e| e.to_string())?.unwrap_or(INITIAL_DIFFICULTY as f64);

        let mut headers = HashMap::new();
        let mut height_index = HashMap::new();
        let mut blocks = HashMap::new();
        let mut cumulative_work = HashMap::new();
        let mut children: HashMap<Hash256, Vec<Hash256>> = HashMap::new();

        let mut cum_work = 0.0;
        for h in 0..=height {
            if let Some(hash) = storage.get_hash_at_height(h).map_err(|e| e.to_string())? {
                if let Some(header) = storage.get_header(&hash).map_err(|e| e.to_string())? {
                    cum_work += block_work(header.difficulty_target);
                    children.entry(header.prev_hash).or_default().push(hash);
                    headers.insert(hash, header);
                }
                if let Some(block) = storage.get_block(&hash).map_err(|e| e.to_string())? {
                    blocks.insert(hash, block);
                }
                cumulative_work.insert(hash, cum_work);
                height_index.insert(h, hash);
            }
        }

        let mut utxo_set = UtxoSet::new();
        for (outpoint, entry) in storage.load_all_utxos().map_err(|e| e.to_string())? {
            utxo_set.add(outpoint, entry);
        }

        tracing::info!("💾 Loaded chain: height={} tip={} utxos={} blocks={}",
            height, &hex::encode(tip)[..16], utxo_set.len(), blocks.len());

        Ok(Chain { headers, blocks, height_index, cumulative_work, children,
            utxo_set, tip, height, recent_timestamps: timestamps,
            fractional_difficulty, storage: Some(storage), batch_mode: false })
    }

    fn persist_genesis(&self, storage: &Storage) -> Result<(), String> {
        let hash = self.tip;
        let genesis = self.blocks.get(&hash).unwrap();
        storage.put_block(&hash, genesis).map_err(|e| e.to_string())?;
        storage.put_header(&hash, &genesis.header).map_err(|e| e.to_string())?;
        storage.put_height_index(0, &hash).map_err(|e| e.to_string())?;
        storage.put_tip(&hash).map_err(|e| e.to_string())?;
        storage.put_height(0).map_err(|e| e.to_string())?;
        storage.put_timestamps(&self.recent_timestamps).map_err(|e| e.to_string())?;
        storage.put_fractional_difficulty(self.fractional_difficulty).map_err(|e| e.to_string())?;
        for (op, entry) in self.utxo_set.iter() {
            storage.put_utxo(op, entry).map_err(|e| e.to_string())?;
        }
        storage.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn create_genesis_block() -> Block {
        let genesis_miner = [0u8; 32];
        let community_fund = [0xCF; 32];
        let reward = block_reward(0);
        let coinbase = Transaction::new_coinbase(0, reward, genesis_miner, community_fund);
        let ts = genesis_timestamp();
        // Genesis version is fixed at 2 (the original protocol version) to ensure
        // the genesis hash never changes when PROTOCOL_VERSION is bumped
        let genesis_version: u32 = 3;
        let merkle_root = {
            let tmp = Block {
                header: BlockHeader {
                    version: genesis_version, prev_hash: NULL_HASH, merkle_root: NULL_HASH,
                    timestamp: ts, difficulty_target: INITIAL_DIFFICULTY,
                    nonce: 0, height: 0,
                },
                transactions: vec![coinbase.clone()],
            };
            tmp.compute_merkle_root()
        };
        Block {
            header: BlockHeader {
                version: genesis_version, prev_hash: NULL_HASH, merkle_root,
                timestamp: ts, difficulty_target: INITIAL_DIFFICULTY,
                nonce: 0, height: 0,
            },
            transactions: vec![coinbase],
        }
    }

    // ─── Block Acceptance ───────────────────────────────────────────

    pub fn add_block(&mut self, block: Block) -> Result<Hash256, BlockError> {
        let block_hash = block.header.hash();

        // 1. Duplicate
        if self.blocks.contains_key(&block_hash) {
            return Err(BlockError::DuplicateBlock);
        }

        // 2. Parent must exist
        let parent_hash = block.header.prev_hash;
        if !self.headers.contains_key(&parent_hash) {
            return Err(BlockError::OrphanBlock);
        }

        // 3. Height must be parent+1
        let parent = self.headers.get(&parent_hash).unwrap();
        let expected_height = parent.height + 1;
        if block.header.height != expected_height {
            return Err(BlockError::InvalidHeight);
        }

        // 4. Timestamp > parent
        if block.header.timestamp <= parent.timestamp {
            return Err(BlockError::InvalidTimestamp);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let is_minimal = block.header.timestamp == parent.timestamp + 1;
        if !is_minimal && block.header.timestamp > now + 7200 {
            return Err(BlockError::TimestampTooFarInFuture);
        }

        // 5. Difficulty: use cached value for tip-extension, full recalc for side chains
        let expected_diff = if parent_hash == self.tip {
            // Extending tip — use the cached fractional difficulty (fast path)
            fractional_to_integer_difficulty(self.fractional_difficulty)
        } else {
            // Side chain — must walk back (slow but correct)
            self.difficulty_for_block_on_parent(&parent_hash)
        };
        if block.header.difficulty_target != expected_diff {
            return Err(BlockError::InvalidDifficulty { expected: expected_diff, got: block.header.difficulty_target });
        }

        // 6. PoW
        if !block.header.meets_difficulty() {
            return Err(BlockError::InsufficientPoW);
        }

        // 7. Merkle root
        if !block.validate_merkle_root() {
            return Err(BlockError::InvalidMerkleRoot);
        }

        // 8. Block size
        if block.size() > MAX_BLOCK_SIZE {
            return Err(BlockError::BlockTooLarge);
        }

        // 9. Basic tx structure
        if block.transactions.is_empty() { return Err(BlockError::NoTransactions); }
        if !block.transactions[0].is_coinbase() { return Err(BlockError::NoCoinbase); }

        // For blocks extending the current tip, do full UTXO validation now.
        let extends_tip = parent_hash == self.tip;

        if extends_tip {
            let expected_reward = block_reward(block.header.height);
            let total_fees = self.calculate_block_fees(&block)?;
            if block.transactions[0].total_output() > expected_reward + total_fees {
                return Err(BlockError::InvalidCoinbaseAmount);
            }
            for tx in &block.transactions[1..] {
                self.validate_transaction(tx, block.header.height)?;
            }

            // Commit directly
            self.apply_block_utxos(&block);
            self.recent_timestamps.push(block.header.timestamp);
            let max_ts = DIFFICULTY_WINDOW + 10;
            if self.recent_timestamps.len() > max_ts {
                self.recent_timestamps.drain(0..self.recent_timestamps.len() - max_ts);
            }
            self.fractional_difficulty = calculate_next_difficulty_fractional(
                self.fractional_difficulty, &self.recent_timestamps);
            self.height_index.insert(block.header.height, block_hash);
            self.tip = block_hash;
            self.height = expected_height;
        }

        // Store block and update indexes
        let parent_work = *self.cumulative_work.get(&parent_hash).unwrap_or(&0.0);
        let new_work = parent_work + block_work(block.header.difficulty_target);
        self.cumulative_work.insert(block_hash, new_work);
        self.headers.insert(block_hash, block.header.clone());
        self.children.entry(parent_hash).or_default().push(block_hash);
        self.blocks.insert(block_hash, block.clone());

        // Check if we need to reorg (side chain has more work than current tip)
        if !extends_tip {
            let tip_work = *self.cumulative_work.get(&self.tip).unwrap_or(&0.0);
            if new_work > tip_work {
                tracing::info!("🔄 Reorg detected! Side chain has more work ({:.0} vs {:.0})", new_work, tip_work);
                self.reorg_to(block_hash)?;
            } else {
                tracing::debug!("📦 Stored side chain block at height {} (work {:.0} vs tip {:.0})",
                    expected_height, new_work, tip_work);
            }
        }

        // Persist
        self.persist_state(&block_hash, &block);

        Ok(block_hash)
    }

    // ─── Reorg ──────────────────────────────────────────────────────

    fn reorg_to(&mut self, new_tip: Hash256) -> Result<(), BlockError> {
        let old_chain = self.chain_from_tip(self.tip);
        let new_chain = self.chain_from_tip(new_tip);

        let old_set: std::collections::HashSet<Hash256> = old_chain.iter().copied().collect();
        let fork_point = new_chain.iter().find(|h| old_set.contains(*h)).copied()
            .ok_or(BlockError::OrphanBlock)?;

        let replay: Vec<Hash256> = new_chain.iter().rev()
            .skip_while(|h| **h != fork_point)
            .skip(1)
            .copied()
            .collect();

        let old_height = self.height;
        let new_height = self.headers.get(&new_tip).unwrap().height;

        tracing::info!("🔄 Reorg: height {} -> {} ({} blocks to replay, fork at {})",
            old_height, new_height, replay.len(), &hex::encode(fork_point)[..16]);

        // Rebuild UTXO set from genesis along the new chain
        self.rebuild_utxo_to(new_tip)?;

        // Update height index for new chain
        self.height_index.clear();
        let full_chain = self.chain_from_tip(new_tip);
        for hash in full_chain.iter().rev() {
            let header = self.headers.get(hash).unwrap();
            self.height_index.insert(header.height, *hash);
        }

        // Update timestamps and difficulty along new chain
        self.recent_timestamps.clear();
        for hash in full_chain.iter().rev() {
            let header = self.headers.get(hash).unwrap();
            self.recent_timestamps.push(header.timestamp);
        }
        let max_ts = DIFFICULTY_WINDOW + 10;
        if self.recent_timestamps.len() > max_ts {
            let drain = self.recent_timestamps.len() - max_ts;
            self.recent_timestamps.drain(0..drain);
        }
        self.fractional_difficulty = INITIAL_DIFFICULTY as f64;
        for ts_window_end in 2..=self.recent_timestamps.len() {
            self.fractional_difficulty = calculate_next_difficulty_fractional(
                self.fractional_difficulty, &self.recent_timestamps[..ts_window_end]);
        }

        self.tip = new_tip;
        self.height = new_height;

        tracing::info!("🔄 Reorg complete. New tip: {} height: {}", &hex::encode(new_tip)[..16], new_height);
        Ok(())
    }

    fn chain_from_tip(&self, tip: Hash256) -> Vec<Hash256> {
        let mut chain = Vec::new();
        let mut current = tip;
        loop {
            chain.push(current);
            if let Some(header) = self.headers.get(&current) {
                if header.prev_hash == NULL_HASH { break; }
                current = header.prev_hash;
            } else {
                break;
            }
        }
        chain
    }

    fn rebuild_utxo_to(&mut self, tip: Hash256) -> Result<(), BlockError> {
        let chain = self.chain_from_tip(tip);
        self.utxo_set = UtxoSet::new();
        for hash in chain.iter().rev() {
            let block = self.blocks.get(hash)
                .ok_or(BlockError::OrphanBlock)?.clone();
            self.apply_block_utxos(&block);
        }
        Ok(())
    }

    // ─── Difficulty ─────────────────────────────────────────────────

    /// Calculate difficulty for a block extending the current tip.
    /// Uses the cached fractional_difficulty — O(1) and always in sync.
    pub fn next_difficulty(&self) -> u32 {
        fractional_to_integer_difficulty(self.fractional_difficulty)
    }

    /// Calculate the expected difficulty for a block whose parent is `parent_hash`.
    /// Walks back along that block's ancestry to gather timestamps.
    /// Used for side-chain validation. O(N) walk.
    pub fn difficulty_for_block_on_parent(&self, parent_hash: &Hash256) -> u32 {
        let mut timestamps = Vec::new();
        let mut current = *parent_hash;

        // Walk back to gather timestamps
        loop {
            if let Some(header) = self.headers.get(&current) {
                timestamps.push(header.timestamp);
                if header.prev_hash == NULL_HASH { break; }
                current = header.prev_hash;
            } else {
                break;
            }
        }

        timestamps.reverse(); // oldest first

        // Replay LWMA to get fractional difficulty at this point
        let mut frac_diff = INITIAL_DIFFICULTY as f64;
        for end in 2..=timestamps.len() {
            frac_diff = calculate_next_difficulty_fractional(frac_diff, &timestamps[..end]);
        }

        fractional_to_integer_difficulty(frac_diff)
    }

    // ─── Block/TX Operations ────────────────────────────────────────

    fn apply_block_utxos(&mut self, block: &Block) {
        for tx in &block.transactions {
            let txid = tx.hash();
            if !tx.is_coinbase() {
                for input in &tx.inputs {
                    self.utxo_set.spend(&input.previous_output);
                }
            }
            for (vout, output) in tx.outputs.iter().enumerate() {
                self.utxo_set.add(
                    OutPoint { txid, vout: vout as u32 },
                    UtxoEntry { output: output.clone(), height: block.header.height, is_coinbase: tx.is_coinbase() },
                );
            }
        }
    }

    fn validate_transaction(&self, tx: &Transaction, block_height: u64) -> Result<(), BlockError> {
        if tx.inputs.is_empty() || tx.outputs.is_empty() {
            return Err(BlockError::InvalidTransaction("empty inputs or outputs".into()));
        }
        let mut input_sum: u64 = 0;
        for (idx, input) in tx.inputs.iter().enumerate() {
            let utxo = self.utxo_set.get(&input.previous_output)
                .ok_or_else(|| BlockError::InvalidTransaction("UTXO not found".into()))?;
            if utxo.is_coinbase && block_height - utxo.height < COINBASE_MATURITY {
                return Err(BlockError::InvalidTransaction("coinbase not mature".into()));
            }
            if input.pubkey.len() != 32 {
                return Err(BlockError::InvalidTransaction(format!("input {} bad pubkey len", idx)));
            }
            let claimed_hash = crate::wallet::pubkey_bytes_to_hash(&input.pubkey);
            if claimed_hash != utxo.output.pubkey_hash {
                return Err(BlockError::InvalidTransaction(format!("input {} pubkey mismatch", idx)));
            }
            let signing_hash = crate::wallet::tx_signing_hash(tx, idx);
            if !crate::wallet::verify_signature(&input.pubkey, &signing_hash, &input.signature) {
                return Err(BlockError::InvalidTransaction(format!("input {} bad signature", idx)));
            }
            input_sum += utxo.output.amount;
        }
        let output_sum = tx.total_output();
        if output_sum > input_sum {
            return Err(BlockError::InvalidTransaction("outputs exceed inputs".into()));
        }
        if input_sum - output_sum < MIN_TX_FEE {
            return Err(BlockError::InvalidTransaction(format!("fee too low: {} < {}", input_sum - output_sum, MIN_TX_FEE)));
        }
        Ok(())
    }

    fn calculate_block_fees(&self, block: &Block) -> Result<u64, BlockError> {
        let mut total_fees: u64 = 0;
        for tx in &block.transactions[1..] {
            let mut input_sum: u64 = 0;
            for input in &tx.inputs {
                let utxo = self.utxo_set.get(&input.previous_output)
                    .ok_or_else(|| BlockError::InvalidTransaction("UTXO not found for fee calc".into()))?;
                input_sum += utxo.output.amount;
            }
            let output_sum = tx.total_output();
            if output_sum > input_sum {
                return Err(BlockError::InvalidTransaction("outputs exceed inputs".into()));
            }
            total_fees += input_sum - output_sum;
        }
        Ok(total_fees)
    }

    // ─── Persistence ────────────────────────────────────────────────

    fn persist_state(&self, block_hash: &Hash256, block: &Block) {
        if self.batch_mode { return; }
        if let Some(ref storage) = self.storage {
            let _ = storage.put_block(block_hash, block);
            let _ = storage.put_header(block_hash, &block.header);
            let _ = storage.put_height_index(self.height, &self.tip);
            let _ = storage.put_tip(&self.tip);
            let _ = storage.put_height(self.height);
            let _ = storage.put_timestamps(&self.recent_timestamps);
            let _ = storage.put_fractional_difficulty(self.fractional_difficulty);
            for tx in &block.transactions {
                if !tx.is_coinbase() {
                    for input in &tx.inputs { let _ = storage.remove_utxo(&input.previous_output); }
                }
                let txid = tx.hash();
                for (vout, _) in tx.outputs.iter().enumerate() {
                    let op = OutPoint { txid, vout: vout as u32 };
                    if let Some(entry) = self.utxo_set.get(&op) { let _ = storage.put_utxo(&op, entry); }
                }
            }
            let _ = storage.flush();
        }
    }

    pub fn set_batch_mode(&mut self, enabled: bool) {
        self.batch_mode = enabled;
    }

    pub fn flush_batch(&self) {
        if let Some(ref storage) = self.storage {
            for (hash, block) in &self.blocks {
                let _ = storage.put_block(hash, block);
                let _ = storage.put_header(hash, &block.header);
            }
            for (h, hash) in &self.height_index {
                let _ = storage.put_height_index(*h, hash);
            }
            let _ = storage.put_tip(&self.tip);
            let _ = storage.put_height(self.height);
            let _ = storage.put_timestamps(&self.recent_timestamps);
            let _ = storage.put_fractional_difficulty(self.fractional_difficulty);
            for (op, entry) in self.utxo_set.iter() {
                let _ = storage.put_utxo(op, entry);
            }
            let _ = storage.flush();
            tracing::info!("💾 Batch flush complete (height {})", self.height);
        }
    }

    // ─── Public Accessors ───────────────────────────────────────────

    pub fn fractional_difficulty(&self) -> f64 { self.fractional_difficulty }

    pub fn block_at_height(&self, height: u64) -> Option<&Block> {
        self.height_index.get(&height).and_then(|h| self.blocks.get(h))
    }

    pub fn header(&self, hash: &Hash256) -> Option<&BlockHeader> { self.headers.get(hash) }

    pub fn tip_header(&self) -> &BlockHeader { self.headers.get(&self.tip).unwrap() }

    pub fn is_persistent(&self) -> bool { self.storage.is_some() }

    pub fn validate_transaction_for_mempool(&self, tx: &Transaction) -> Result<(), BlockError> {
        if tx.is_coinbase() {
            return Err(BlockError::InvalidTransaction("coinbase not allowed in mempool".into()));
        }
        self.validate_transaction(tx, self.height + 1)
    }

    pub fn total_known_blocks(&self) -> usize { self.blocks.len() }

    pub fn block_by_hash(&self, hash: &Hash256) -> Option<&Block> { self.blocks.get(hash) }

    pub fn headers_in_range(&self, start: u64, count: u32) -> Vec<BlockHeader> {
        let mut headers = Vec::new();
        let end = (start + count as u64).min(self.height + 1);
        for h in start..end {
            if let Some(hash) = self.height_index.get(&h) {
                if let Some(header) = self.headers.get(hash) {
                    headers.push(header.clone());
                }
            }
        }
        headers
    }

    pub fn validate_header_chain(&self, headers: &[BlockHeader]) -> Vec<Hash256> {
        let mut valid = Vec::new();
        let mut prev_hash = if let Some(first) = headers.first() {
            first.prev_hash
        } else {
            return valid;
        };

        for header in headers {
            let hash = header.hash();

            if self.headers.contains_key(&hash) {
                prev_hash = hash;
                valid.push(hash);
                continue;
            }

            if !self.headers.contains_key(&header.prev_hash) && header.prev_hash != prev_hash {
                break;
            }

            if !header.meets_difficulty() {
                break;
            }

            prev_hash = hash;
            valid.push(hash);
        }
        valid
    }

    pub fn blocks_by_hashes(&self, hashes: &[Hash256]) -> Vec<Block> {
        hashes.iter()
            .filter_map(|h| self.blocks.get(h).cloned())
            .collect()
    }

    pub fn genesis_hash(&self) -> Hash256 {
        self.height_index.get(&0).copied().unwrap_or(NULL_HASH)
    }
}

// ─── Errors ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum BlockError {
    DuplicateBlock, OrphanBlock, InvalidHeight, InvalidPrevHash,
    InvalidTimestamp, TimestampTooFarInFuture,
    InvalidDifficulty { expected: u32, got: u32 },
    InsufficientPoW, InvalidMerkleRoot, BlockTooLarge,
    NoTransactions, NoCoinbase, InvalidCoinbaseAmount,
    InvalidTransaction(String),
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::DuplicateBlock => write!(f, "duplicate block"),
            BlockError::OrphanBlock => write!(f, "orphan block"),
            BlockError::InvalidHeight => write!(f, "invalid height"),
            BlockError::InvalidPrevHash => write!(f, "prev_hash mismatch"),
            BlockError::InvalidTimestamp => write!(f, "invalid timestamp"),
            BlockError::TimestampTooFarInFuture => write!(f, "timestamp too far in future"),
            BlockError::InvalidDifficulty { expected, got } => write!(f, "difficulty mismatch ({} vs {})", expected, got),
            BlockError::InsufficientPoW => write!(f, "insufficient PoW"),
            BlockError::InvalidMerkleRoot => write!(f, "invalid merkle root"),
            BlockError::BlockTooLarge => write!(f, "block too large"),
            BlockError::NoTransactions => write!(f, "no transactions"),
            BlockError::NoCoinbase => write!(f, "no coinbase"),
            BlockError::InvalidCoinbaseAmount => write!(f, "coinbase amount too large"),
            BlockError::InvalidTransaction(msg) => write!(f, "invalid tx: {}", msg),
        }
    }
}
impl std::error::Error for BlockError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chain_genesis() {
        let chain = Chain::new();
        assert_eq!(chain.height, 0);
        assert!(!chain.utxo_set.is_empty());
    }

    #[test]
    fn test_initial_difficulty() {
        let chain = Chain::new();
        assert_eq!(chain.next_difficulty(), INITIAL_DIFFICULTY);
    }

    #[test]
    fn test_cumulative_work() {
        let chain = Chain::new();
        let genesis_hash = chain.tip;
        let work = chain.cumulative_work.get(&genesis_hash).unwrap();
        assert!(*work > 0.0);
    }
}